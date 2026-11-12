"""JEAN desktop overlay — voice slice.

Project 3's cyan ring and particle field sit at the centre, rendered
with QPainter. Microphone RMS drives it during Talk. The Windows playback
meter drives it when audio plays through the default output device.

Voice in: push Talk, speak, push Talk again (or stay silent past the
endpoint). The captured 16 kHz WAV is decoded by the selected backend —
project 9's Faster-Whisper base.en by default, or project 2's
Zipformer+Parakeet hybrid — then routed by mode:

  * Chat  → `jean chat <text>`        (read-only conversation, never a task)
  * Task  → the Rust desktop request router

Chat replies are spoken verbatim. Long task results are shortened by
the Rust `jean speak --task-result` command before Windows Speech plays them. Stop halts
playback; provider/playback failures are surfaced in the status line.
"""
import ctypes
from ctypes import wintypes
import json
import math
import os
import re
import secrets
import subprocess
import sys
import tempfile
import threading
import time
import winreg
from dataclasses import dataclass
from datetime import datetime
from pathlib import Path
from urllib.parse import urlencode

import voice_bench
from audio_meter import PlaybackMeter
from workspace_trash import WorkspaceTrash

from PyQt6.QtCore import (Qt, QTimer, QPoint, QPointF, QRectF, QObject, QProcess, QSettings, QUrl,
                          QAbstractNativeEventFilter, pyqtSignal)
from PyQt6.QtGui import QBrush, QColor, QPainter, QPainterPath, QRadialGradient, QPen, QDesktopServices
from PyQt6.QtWidgets import (QApplication, QWidget, QVBoxLayout, QHBoxLayout,
                             QLabel, QPushButton, QLineEdit, QFrame,
                             QGraphicsDropShadowEffect, QComboBox, QFileDialog,
                             QMessageBox, QDialog, QPlainTextEdit)

ROOT = Path(__file__).resolve().parents[2]
HERE = Path(__file__).resolve().parent
EXE = ROOT / "target" / "debug" / "jean.exe"
NO_WINDOW = getattr(subprocess, "CREATE_NO_WINDOW", 0)
TALK_HOTKEY_ID = 1
WM_HOTKEY = 0x0312
MOD_SHIFT = 0x0004
MOD_NOREPEAT = 0x4000
VK_CAPITAL = 0x14
PROVIDER_NONE_MARKER = 'model.provider = "none"'
CHAT_SETUP_TITLE = "Chat needs a model provider"
CHAT_SETUP_HINT = (
    'Set provider (e.g. "ollama"), model and base_url under [model] in '
    "%LOCALAPPDATA%\\JEAN\\jean.toml (or <JEAN_DATA_DIR>\\jean.toml), "
    "then verify with `jean model ping`. Tool and delegated worker tasks "
    "run without a provider; natural-language planning needs one."
)

WAKE_NAME = r"(?:j\.?\s*e\.?\s*a\.?\s*n\.?|gene|dean|gee|g)"
WAKE_ADDRESS = re.compile(rf"^(?:hey\s+)?{WAKE_NAME}(?:[\s,.:!?]+|$)", re.IGNORECASE)
REQUEST_PREFIX = re.compile(
    r"^(?:(?:please|can you|could you|would you|"
    r"i want you to|i need you to|i would like you to|i'd like you to)[, ]+)+",
    re.IGNORECASE,
)
ACTION_REQUEST = re.compile(
    r"(?:open|browse|navigate|go to|visit|search|look up|run|execute|build|test|"
    r"create|write|edit|fix|update|install|remove|delete|move|copy|launch|start|"
    r"check|implement|find|delegate|play|watch|put on|clean|empty|restore)\b", re.IGNORECASE
)
BROWSER_PHRASE = (
    r"\s+(?:(?:in|on|using|with)\s+)?(?:(?:my|the)\s+)?"
    r"(?:default\s+)?(?:web\s+)?browser"
)
SITE_REQUEST = re.compile(
    r"(?:open|launch|visit|go to|navigate to|play|watch)\s+(?:up\s+)?(?:the\s+)?"
    r"(?P<destination>.+?)(?P<browser>" + BROWSER_PHRASE + r")?"
    r"(?:\s+for me)?(?:,?\s+please)?\.?",
    re.IGNORECASE,
)


def site_request_text(text):
    return REQUEST_PREFIX.sub("", WAKE_ADDRESS.sub("", text.strip())).rstrip("?!")


def web_request(text):
    text = site_request_text(text)
    text = re.sub(r"\byou\s+tube\b", "YouTube", text, flags=re.IGNORECASE)
    if re.fullmatch(r"YouTube(?: please)?\.?", text, re.IGNORECASE):
        return "https://www.youtube.com/"
    match = SITE_REQUEST.fullmatch(text)
    if match:
        address = match["destination"]
        if re.fullmatch(r"https?://\S+", address, re.IGNORECASE):
            return address
        if (re.fullmatch(r"youtube|your dip|your ear to me|your u[ -]d", address, re.IGNORECASE)
                or (match["browser"] and re.fullmatch(
                    r"(?:your\s+)?dear[ -]?bun", address, re.IGNORECASE
                ))):
            return "https://www.youtube.com/"
        if match["browser"]:
            return None
    match = re.fullmatch(
        r"(?:play|watch|put on)(?:\s+me)?\s+(.+?)(?:\s+on\s+youtube)?\.?",
        text, re.IGNORECASE,
    )
    if not match:
        match = re.fullmatch(r"(?:search|look)\s+(?:up\s+|for\s+)?(.+?)\s+on\s+youtube\.?",
                             text, re.IGNORECASE)
    if match:
        return "https://www.youtube.com/results?" + urlencode({"search_query": match[1]})
    return None


def gmail_request(text):
    text = REQUEST_PREFIX.sub("", WAKE_ADDRESS.sub("", text.strip())).rstrip(".?!")
    return bool(re.fullmatch(
        r"(?:open|launch|check|show(?: me)?|read)\s+(?:up\s+)?"
        r"(?:(?:my|the|installed|unread)\s+)*(?:g\s*mail|e[ -]?mails?|mails?|inbox)"
        r"(?:\s+app)?(?:\s+for me)?",
        text, re.IGNORECASE,
    ))


def clock_request(text):
    text = REQUEST_PREFIX.sub("", WAKE_ADDRESS.sub("", text.strip()))
    text = " ".join(text.casefold().replace("’", "'").rstrip(".?!").split())
    place = r"sri lanka|colombo"
    match = re.fullmatch(
        r"(?:/time|what time is it|"
        r"(?:(?:what(?:'s| is)|tell me|show me|give me|check|get|i need|i want) )?"
        rf"(?:(?:the|my|current|local|device|system|computer|windows|laptop|{place}) )*"
        r"(?:time|clock))"
        rf"(?:(?: (?:right )?now)|(?: (?:in|on|from|for) (?:{place}|"
        r"(?:my|this|the) (?:device|pc|computer|laptop|windows laptop))))*",
        text,
    )
    if match is None:
        return None
    return "sri-lanka" if re.search(rf"\b(?:{place})\b", text) else "device"


def clock_answer(scope):
    if scope == "sri-lanka":
        # The Windows timezone database is available even without Python tzdata.
        result = subprocess.run([
            "powershell.exe", "-NoProfile", "-NonInteractive", "-Command",
            "[TimeZoneInfo]::ConvertTimeBySystemTimeZoneId([DateTimeOffset]::Now,"
            "'Sri Lanka Standard Time').ToString('h:mm tt',"
            "[Globalization.CultureInfo]::InvariantCulture)",
        ], check=True, capture_output=True, text=True, timeout=5, creationflags=NO_WINDOW)
        return f"It's {result.stdout.strip()} in Sri Lanka."
    return f"It's {datetime.now().astimezone().strftime('%I:%M %p').lstrip('0')}."


WORKFLOW_NAME = r"[\w][\w -]{0,47}"
WORKFLOW_TEACH = re.compile(rf"remember workflow (?P<name>{WORKFLOW_NAME}?) as (?P<steps>.+)",
                            re.IGNORECASE)
WORKFLOW_COMMAND = re.compile(rf"(?P<verb>run|forget|undo|show) workflow (?P<name>{WORKFLOW_NAME}?)"
                               r"(?: with (?P<input>query|url) (?P<value>.+))?",
                               re.IGNORECASE)
WORKFLOW_TEMPLATES = {
    "open {url}": "url",
    "search {query} on youtube": "query",
}


def workflow_step(text):
    slot = WORKFLOW_TEMPLATES.get(text.casefold())
    if slot:
        return ["input", slot]
    scope = clock_request(text)
    if scope:
        return ["clock", scope]
    if gmail_request(text):
        return ["gmail", ""]
    address = web_request(text)
    if address and valid_web_url(address):
        return ["browser", address]
    return None


def valid_web_url(address):
    if not isinstance(address, str):
        return False
    url = QUrl(address)
    return url.isValid() and url.scheme() in {"https", "http"} and bool(url.host()) and not url.userInfo()


def valid_workflow_step(step):
    if not isinstance(step, list) or len(step) != 2:
        return False
    kind, value = step
    return ((kind == "clock" and value in ("device", "sri-lanka"))
            or (kind == "gmail" and value == "")
            or (kind == "input" and value in ("query", "url"))
            or (kind == "browser" and valid_web_url(value)))


def valid_workflow(steps):
    return (isinstance(steps, list) and 1 <= len(steps) <= 5
            and all(map(valid_workflow_step, steps))
            and len({value for kind, value in steps if kind == "input"}) <= 1)


def workflow_revisions(raw):
    try:
        data = json.loads(raw)
    except (ValueError, TypeError):
        return []
    if valid_workflow(data):
        return [data]
    if isinstance(data, dict) and data.get("version") == 1:
        revisions = data.get("revisions")
        if isinstance(revisions, list) and 1 <= len(revisions) <= 8 and all(map(valid_workflow, revisions)):
            return revisions
    return []


def bind_workflow(steps, supplied, value):
    slot = next((value for kind, value in steps if kind == "input"), None)
    if slot is None:
        if supplied:
            raise ValueError("This workflow takes no input.")
        return steps
    if not supplied or supplied.casefold() != slot or not value.strip():
        raise ValueError(f"This workflow needs “with {slot} …”.")
    value = value.strip()
    if slot == "query":
        if len(value) > 250:
            raise ValueError("Use a search query of 250 characters or fewer.")
        value = "https://www.youtube.com/results?" + urlencode({"search_query": value})
    if not valid_web_url(value):
        raise ValueError("Enter a valid HTTP or HTTPS URL.")
    return [["browser", value] if kind == "input" else [kind, argument] for kind, argument in steps]


def trash_request(text):
    text = REQUEST_PREFIX.sub("", text.strip())
    command = text.rstrip(".?!").casefold()
    if command in {"/trash", "/trash list", "show trash", "list trash", "show the trash",
                   "what is in the trash", "what's in the trash"}:
        return "list", ""
    if command in {"/trash clean", "clean trash", "empty trash", "clean the trash",
                   "empty the trash"}:
        return "clean", ""
    for pattern, action in (
        (r"/trash restore\s+(.+)", "restore"),
        (r"/trash put\s+(.+)", "put"),
        (r"delete\s+(?:(?:the\s+)?(?:file|folder)\s+)?(.+)", "put"),
    ):
        match = re.fullmatch(pattern, text, re.IGNORECASE)
        if match:
            return action, match[1].strip().strip('"')
    return None


def request_mode(mode, text):
    if mode != "smart":
        return mode
    return ("task" if text.startswith("/") or web_request(text) or gmail_request(text)
            or ACTION_REQUEST.match(REQUEST_PREFIX.sub("", WAKE_ADDRESS.sub("", text)))
            or trash_request(text) else "chat")

@dataclass
class VoiceTurn:
    mode: str
    backend: str
    task: object
    directory: tempfile.TemporaryDirectory
    released_at: float | None = None
    abandoned: bool = False
    asr_fallback: bool = False
    approval: object = None


class Bridge(QObject):
    task_retry = pyqtSignal(str)
    result = pyqtSignal(str, str)      # title, detail
    transcript = pyqtSignal(str)       # heard / reply / task text
    level = pyqtSignal(float)          # measured mic level 0..1
    capture_ready = pyqtSignal(object)
    capture_finished = pyqtSignal(object, str, float)
    decoded = pyqtSignal(object, str, str, int)
    speak_requested = pyqtSignal(str, bool)  # text, task result
    work_finished = pyqtSignal()
    playback_finished = pyqtSignal(object, str)  # process, error text
    task_status = pyqtSignal(str, str)
    latest_task = pyqtSignal(str)
    chat_setup = pyqtSignal(str)     # full provider diagnostic; empty = configured


class TalkHotkey(QAbstractNativeEventFilter):
    def __init__(self, activate):
        super().__init__()
        self.activate = activate

    def nativeEventFilter(self, event_type, message):
        if event_type in (b"windows_generic_MSG", b"windows_dispatcher_MSG"):
            msg = wintypes.MSG.from_address(int(message))
            if msg.message == WM_HOTKEY and msg.wParam == TALK_HOTKEY_ID:
                QTimer.singleShot(0, self.activate)
                return True, 0
        return False, 0


class Reactor(QWidget):
    """Native rendering of Project 3's ring, sweep, and dust."""

    toggle_message_requested = pyqtSignal()
    ACCENT = (0, 229, 255)

    def __init__(self):
        super().__init__()
        self.phase = 0.0
        self.level = 0.0
        self.target = 0.0
        self.setFixedSize(200, 200)
        self.honeycomb = self._honeycomb()
        self.traces = QPainterPath()
        self.contacts = []
        for x, y, direction in ((11, -21, 1), (-16, 7, -1), (6, 17, 1), (-10, -6, -1)):
            end = x + 21 * direction
            self.traces.moveTo(x, y)
            self.traces.lineTo(x + 8 * direction, y)
            self.traces.lineTo(x + 13 * direction, y + 5)
            self.traces.lineTo(end, y + 5)
            self.contacts.append(QPointF(end, y + 5))
        import random
        rng = random.Random(20261111)
        # Particles cluster around the rim and thin toward the edge, as in
        # Core.tsx. A second population drifts through the surrounding field.
        self.dust = [
            (rng.uniform(0, math.tau), rng.gauss(3, 6), rng.random(), rng.uniform(0.3, 1.15))
            for _ in range(420)
        ]
        self.field = [
            (rng.uniform(0, math.tau), rng.uniform(18, 92), rng.random(), rng.uniform(0.35, 0.8))
            for _ in range(110)
        ]
        self.timer = QTimer(self)
        self.timer.timeout.connect(self.animate)
        self.timer.start(33)

    @staticmethod
    def _honeycomb():
        path = QPainterPath()
        size = 6.5
        row_height = math.sqrt(3) * size
        for column in range(-6, 7):
            for row in range(-5, 6):
                x = column * size * 1.5
                y = (row + 0.5 * (column % 2)) * row_height
                for corner in range(6):
                    angle = corner * math.tau / 6
                    point = QPointF(x + size * math.cos(angle), y + size * math.sin(angle))
                    if corner == 0:
                        path.moveTo(point)
                    else:
                        path.lineTo(point)
                path.closeSubpath()
        return path

    def _paint_inner_plate(self, painter, radius, level):
        r = radius - 10
        bounds = QRectF(-r, -r, 2 * r, 2 * r)
        clip = QPainterPath()
        clip.addEllipse(bounds)
        painter.save()
        painter.setClipPath(clip)

        glass = QRadialGradient(-9, -13, r * 1.35)
        glass.setColorAt(0, QColor(22, 18, 12, 122))
        glass.setColorAt(0.68, QColor(6, 7, 8, 163))
        glass.setColorAt(1, QColor(0, 0, 0, 207))
        painter.setPen(Qt.PenStyle.NoPen)
        painter.setBrush(glass)
        painter.drawEllipse(bounds)

        gold = QRadialGradient(-9, -13, r)
        gold.setColorAt(0, QColor(216, 166, 79, 70 + round(level * 24)))
        gold.setColorAt(0.6, QColor(185, 133, 52, 49))
        gold.setColorAt(1, QColor(145, 99, 32, 7))
        painter.setBrush(Qt.BrushStyle.NoBrush)
        painter.setPen(QPen(QBrush(gold), 0.55))
        painter.drawPath(self.honeycomb)

        painter.setPen(QPen(QColor(223, 169, 72, 82 + round(level * 36)), 0.8))
        painter.drawPath(self.traces)
        for contact in self.contacts:
            painter.drawEllipse(contact, 1.5, 1.5)
        painter.restore()

    def mousePressEvent(self, event):
        if event.button() == Qt.MouseButton.RightButton:
            self.toggle_message_requested.emit()
            event.accept()
            return
        if event.button() == Qt.MouseButton.LeftButton:
            handle = self.window().windowHandle()
            if handle is not None and handle.startSystemMove():
                event.accept()
                return
            self.drag_offset = event.globalPosition().toPoint() - self.window().pos()
            event.accept()
            return
        super().mousePressEvent(event)

    def mouseMoveEvent(self, event):
        offset = getattr(self, "drag_offset", None)
        if offset is not None and event.buttons() & Qt.MouseButton.LeftButton:
            self.window().move(event.globalPosition().toPoint() - offset)
            event.accept()
            return
        super().mouseMoveEvent(event)

    def mouseReleaseEvent(self, event):
        self.drag_offset = None
        super().mouseReleaseEvent(event)

    def animate(self):
        self.phase += 0.055
        breathe = (math.sin(self.phase * 0.9) * 0.5 + 0.5) * 0.12
        want = max(min(1.0, self.target), breathe)
        self.level += (want - self.level) * (0.58 if want > self.level else 0.16)
        self.update()

    def paintEvent(self, event):
        painter = QPainter(self)
        painter.setRenderHint(QPainter.RenderHint.Antialiasing)
        center = QPointF(self.width() / 2, self.height() / 2)
        painter.translate(center)
        level = self.level
        radius = 61.0 + level * 14.0
        sweep = self.phase * 0.55

        def color(alpha):
            return QColor(*self.ACCENT, int(alpha))

        def circle(r):
            return QRectF(-r, -r, 2 * r, 2 * r)

        self._paint_inner_plate(painter, radius, level)

        # Teal wash concentrated around the rim.
        glow = QRadialGradient(0.0, 0.0, 99.0)
        for offset, alpha in ((0.0, 0), (28.0, 0), (radius - 13, 18),
                              (radius, 85 + 50 * level), (radius + 9, 42),
                              (radius + 24, 0), (99.0, 0)):
            glow.setColorAt(offset / 99.0, color(alpha))
        painter.setPen(Qt.PenStyle.NoPen)
        painter.setBrush(glow)
        painter.drawEllipse(circle(99.0))

        # Core.tsx's complete outer ring, recessed inner wall and hairlines.
        painter.setBrush(Qt.BrushStyle.NoBrush)
        for r, alpha, width in ((radius - 16, 26, 0.65), (radius - 9, 47, 0.8),
                                (radius - 4, 98, 1.7), (radius, 125, 2.0),
                                (radius + 2, 67, 0.8)):
            painter.setPen(QPen(color(alpha + level * 35), width))
            painter.drawEllipse(circle(r))

        # Tapered travelling highlight, with a short luminous leading edge.
        for segment in range(24):
            strength = ((segment + 1) / 24) ** 2
            angle = -math.degrees(sweep) + (23 - segment) * 4
            painter.setPen(QPen(QColor(126, 245, 255, int(160 * strength)), 1.5 + strength * 2))
            painter.drawArc(circle(radius), round(angle * 16), 5 * 16)

        painter.setPen(Qt.PenStyle.NoPen)
        for angle, distance, seed, size in self.field:
            angle += self.phase * (0.08 + seed * 0.08)
            r = min(96.0, distance * (1.0 + level * 0.12))
            painter.setBrush(color(18 + seed * 49 + level * 34))
            painter.drawEllipse(QPointF(math.cos(angle) * r, math.sin(angle) * r), size, size)
        for angle, offset, seed, size in self.dust:
            angle += self.phase * (0.045 + seed * 0.045)
            r = radius + offset + math.sin(angle * 8 + self.phase + seed) * 1.8
            ahead = (sweep - angle) % math.tau
            wake = max(0.0, 1.0 - ahead / 1.65)
            flicker = 0.5 + 0.5 * math.sin(self.phase * 2.0 + seed * 37)
            alpha = 24 + seed * 104 + wake * 66 + level * 25 + flicker * 18
            painter.setBrush(QColor(42, 224, 255, int(alpha)))
            painter.drawEllipse(QPointF(math.cos(angle) * r, math.sin(angle) * r), size, size)


class Overlay(QWidget):
    def __init__(self):
        super().__init__()
        self.setWindowTitle("JEAN")
        self.setWindowFlags(Qt.WindowType.FramelessWindowHint | Qt.WindowType.WindowStaysOnTopHint | Qt.WindowType.Window)
        self.setAttribute(Qt.WidgetAttribute.WA_TranslucentBackground)
        self.resize(370, 330)
        self.bridge = Bridge()
        self.bridge.result.connect(self.show_result)
        self.bridge.transcript.connect(self.show_transcript)
        self.bridge.level.connect(self.on_level)
        self.bridge.capture_ready.connect(self._on_capture_ready)
        self.bridge.capture_finished.connect(self._on_capture_finished)
        self.bridge.decoded.connect(self._on_decoded)
        self.bridge.speak_requested.connect(self._speak_async)
        self.bridge.work_finished.connect(self._finish_busy_ui)
        self.bridge.playback_finished.connect(self._on_playback_finished)
        self.bridge.task_status.connect(self._show_task_status)
        self.bridge.latest_task.connect(self._show_latest_task)
        self.bridge.chat_setup.connect(self._on_chat_setup)
        self.bridge.task_retry.connect(self._offer_task_retry)

        # Voice state
        self.foreground_cancel = None
        self.activity_lines = []
        self.last_local_step = None
        self.last_open_step = None
        self.mode = "smart"
        data_dir = Path(os.environ.get("JEAN_DATA_DIR") or Path(os.environ["LOCALAPPDATA"]) / "JEAN")
        self.trash = WorkspaceTrash(data_dir / "workspace-trash")
        self.backend = voice_bench.DEFAULT_BACKEND
        self.listening = False
        self.capture_proc = None
        self.voice_turn = None
        self.mic_level = 0.0
        self.speaking_proc = None
        self._stopped_speech = set()
        self.busy = False
        self.task_proc = None
        self.approval = None
        self.chat_setup_detail = ""
        self.status_pending = set()
        self.spoken_status = set()
        try:
            self.playback_meter = PlaybackMeter()
        except OSError:
            self.playback_meter = None

        outer = QVBoxLayout(self)
        outer.setContentsMargins(16, 0, 16, 14)
        outer.setSpacing(0)
        orbrow = QHBoxLayout()
        orbrow.addStretch()
        self.reactor = Reactor()
        self.reactor.toggle_message_requested.connect(self.toggle_message)
        orbrow.addWidget(self.reactor)
        orbrow.addStretch()
        outer.addLayout(orbrow)
        self.card = QFrame()
        self.card.setObjectName("card")
        shadow = QGraphicsDropShadowEffect(self.card)
        shadow.setBlurRadius(28)
        shadow.setColor(QColor(0, 0, 0, 95))
        shadow.setOffset(0, 8)
        self.card.setGraphicsEffect(shadow)
        box = QVBoxLayout(self.card)
        box.setContentsMargins(19, 16, 19, 16)
        box.setSpacing(8)
        top = QHBoxLayout()
        badge = QLabel("✦  JEAN")
        badge.setObjectName("badge")
        top.addWidget(badge)
        top.addStretch()
        self.status = QLabel("●  READY")
        self.status.setObjectName("status")
        top.addWidget(self.status)
        box.addLayout(top)
        self.title = QLabel("Here when you need me")
        self.title.setObjectName("title")
        self.title.setWordWrap(True)
        box.addWidget(self.title)
        self.detail = QLabel("Ask a question or tell J.E.A.N. what to do.")
        self.detail.setObjectName("detail")
        self.detail.setWordWrap(True)
        self.detail.setMaximumHeight(self.detail.fontMetrics().lineSpacing() * 5)
        self.detail.setTextFormat(Qt.TextFormat.PlainText)
        box.addWidget(self.detail)
        controls = QHBoxLayout()
        controls.setSpacing(6)
        self.talk = QPushButton("● Talk")
        self.talk.setObjectName("mini")
        self.talk.setToolTip("Start or stop microphone capture")
        self.talk.clicked.connect(self.toggle_listen)
        controls.addWidget(self.talk)
        self.mode_btn = QPushButton("Mode: Smart")
        self.mode_btn.setObjectName("mini")
        self.mode_btn.setToolTip("Switch between Smart, Task, and Chat")
        self.mode_btn.clicked.connect(self.toggle_mode)
        controls.addWidget(self.mode_btn)
        self.backend_btn = QPushButton("ASR: Project 9")
        self.backend_btn.setObjectName("mini")
        self.backend_btn.setToolTip("Voice decode backend: project 9 Faster-Whisper or project 2 Zipformer+Parakeet")
        self.backend_btn.clicked.connect(self.toggle_backend)
        controls.addWidget(self.backend_btn)
        self.stop_speech = QPushButton("Stop speech")
        self.stop_speech.setObjectName("mini")
        self.stop_speech.setToolTip("Stop spoken playback")
        self.stop_speech.clicked.connect(self.stop_speech_playback)
        self.stop_speech.setEnabled(False)
        controls.addWidget(self.stop_speech)
        box.addLayout(controls)
        self.wake_button = QPushButton("Wake: On")
        self.wake_button.setObjectName("mini")
        self.wake_button.setCheckable(True)
        self.wake_button.setChecked(True)
        self.wake_button.setToolTip(
            "Local microphone detection while idle. Say Jean, Dean, Gene, or G."
        )
        self.wake_button.toggled.connect(self._set_wake_enabled)
        voice_row = QHBoxLayout()
        voice_row.addWidget(self.wake_button)
        self.voice_settings = QSettings("JEAN", "Desktop")
        self.voice_choice = QComboBox()
        self.voice_choice.setToolTip("Voice used for spoken replies")
        self.voice_choice.addItem("Voice: Windows default", "")
        self._load_voices()
        voice_row.addWidget(self.voice_choice, 1)
        box.addLayout(voice_row)
        action_row = QHBoxLayout()
        self.yolo = QPushButton("Ask for approval")
        self.yolo.setCheckable(True)
        self.yolo.setChecked(self.voice_settings.value("yolo", False, type=bool))
        if self.yolo.isChecked():
            self.yolo.setText("YOLO: On")
        self.yolo.setToolTip("Switch between asking for approval and YOLO")
        self.yolo.setStyleSheet("QPushButton:checked { background: #f0c64d; color: #18150c; }")
        self.yolo.toggled.connect(self._set_yolo)
        action_row.addWidget(self.yolo)
        stop = QPushButton("Terminate")
        stop.clicked.connect(self._terminate)
        action_row.addWidget(stop)
        history = QPushButton("Activity")
        history.clicked.connect(self._show_activity)
        action_row.addWidget(history)
        self.voice_rate = QComboBox()
        self.voice_rate.setToolTip("Speaking pace")
        for label, rate in (("Natural pace", 0), ("Slower pace", -2), ("Faster pace", 2)):
            self.voice_rate.addItem(label, rate)
        saved_rate = self.voice_settings.value("rate", 0, type=int)
        self.voice_rate.setCurrentIndex(max(0, self.voice_rate.findData(saved_rate)))
        self.voice_rate.currentIndexChanged.connect(
            lambda: self.voice_settings.setValue("rate", self.voice_rate.currentData())
        )
        action_row.addWidget(self.voice_rate)
        for button in (self.yolo, stop, history):
            button.setObjectName("mini")
        box.addLayout(action_row)
        row = QHBoxLayout()
        self.entry = QLineEdit()
        self.entry.setPlaceholderText("Ask J.E.A.N. or give it a task…")
        self.entry.returnPressed.connect(self.submit)
        row.addWidget(self.entry)
        send = QPushButton("↑")
        send.setObjectName("send")
        send.setToolTip("Send using the selected mode")
        send.clicked.connect(self.submit)
        row.addWidget(send)
        box.addLayout(row)
        self.task_options = QWidget()
        options = QVBoxLayout(self.task_options)
        options.setContentsMargins(0, 0, 0, 0)
        workspace_row = QHBoxLayout()
        self.workspace = QLineEdit()
        self.workspace.setPlaceholderText("Configured workspace")
        workspace_row.addWidget(self.workspace)
        browse = QPushButton("Browse…")
        browse.clicked.connect(self._choose_workspace)
        workspace_row.addWidget(browse)
        options.addLayout(workspace_row)
        agent_row = QHBoxLayout()
        self.worker = QLineEdit()
        self.worker.setPlaceholderText("Worker (optional)")
        agent_row.addWidget(self.worker)
        self.profile = QComboBox()
        self.profile.addItem("Default profile", "")
        self.profile.addItem("Quick prototype", "quick-prototype")
        self.profile.addItem("Polished UI", "polished-ui")
        agent_row.addWidget(self.profile)
        options.addLayout(agent_row)
        self.instruction = QLineEdit()
        self.instruction.setPlaceholderText("Task-specific quality instruction")
        options.addWidget(self.instruction)
        box.addWidget(self.task_options)
        task_row = QHBoxLayout()
        self.task_id = QLineEdit()
        self.task_id.setPlaceholderText("Task ID")
        task_row.addWidget(self.task_id)
        for label, action in (
            ("Status", self._request_task_status),
            ("Resume", self._resume_task),
            ("Review", self._review_task),
            ("Cancel task", self._cancel_task),
        ):
            button = QPushButton(label)
            button.setObjectName("mini")
            button.clicked.connect(lambda checked=False, callback=action: callback())
            task_row.addWidget(button)
            if label == "Cancel task":
                self.cancel_task_button = button
                button.setEnabled(False)
        box.addLayout(task_row)
        self.activity = QLabel()
        self.activity.setTextFormat(Qt.TextFormat.PlainText)
        self.activity.setWordWrap(True)
        box.addWidget(self.activity)
        self.poll_task = QTimer(self)
        self.poll_task.setInterval(1500)
        self.poll_task.timeout.connect(self._request_task_status)
        self.note = QLabel("Task examples: Start with Codex. Fix the test. · /delegate Fix the test")
        self.note.setObjectName("note")
        self.note.setWordWrap(True)
        box.addWidget(self.note)
        outer.addWidget(self.card)
        self.card.hide()
        toggle_row = QHBoxLayout()
        toggle_row.addStretch()
        self.toggle = QPushButton("Message")
        self.toggle.setObjectName("toggle")
        self.toggle.clicked.connect(self.toggle_message)
        toggle_row.addWidget(self.toggle)
        toggle_row.addStretch()
        outer.addLayout(toggle_row)
        self.setStyleSheet("""
            #card { background: rgba(24, 28, 44, 242); border: 1px solid rgba(183, 178, 242, 48); border-radius: 22px; }
            QLabel { background: transparent; color: #f5f4ff; border: none; }
            #badge { color: #c8bfff; font-size: 11px; font-weight: 700; letter-spacing: 2px; }
            #status { color: #91dfc5; font-size: 10px; font-weight: 700; }
            #title { font-size: 17px; font-weight: 700; }
            #detail { color: #adb5cd; font-size: 12px; }
            #note { color: #8f9ab5; font-size: 10px; }
            QLineEdit { color: #f8f7ff; background: rgba(255,255,255,14); border: 1px solid rgba(205,200,255,38); border-radius: 14px; padding: 10px 12px; font-size: 12px; }
            QLineEdit:focus { border: 1px solid #a79aff; }
            #toggle { background: rgba(24,28,44,230); color: #e7e2ff; border: 1px solid rgba(205,200,255,50); border-radius: 15px; padding: 7px 18px; font-size: 11px; }
            #toggle:hover { background: #403b68; }
            #send { background: #a99aff; color: #17152c; border: none; border-radius: 15px; font-size: 21px; font-weight: bold; min-width: 38px; min-height: 38px; }
            #send:hover { background: #c3b9ff; }
            #mini { background: rgba(255,255,255,10); color: #cfc9ff; border: 1px solid rgba(205,200,255,40); border-radius: 11px; padding: 4px 9px; font-size: 10px; }
            #mini:hover { background: #3a3560; }
            #mini:disabled { color: #6d7390; border-color: rgba(205,200,255,18); }
        """)
        self.anim = QTimer(self)
        self.anim.timeout.connect(self.tick)
        self.anim.start(50)
        screen = QApplication.primaryScreen().availableGeometry()
        self.move(screen.center().x() - self.width() // 2, screen.center().y() - self.height() // 2)
        self.refresh()
        self.wake_proc = None
        self.wake_terminate_only = False
        self.wake_approval = None
        self.approval_listener_failed = None
        self.output_quiet_at = 0.0
        self.output_level = 0.0
        self.hotkey_window = wintypes.HWND(int(self.winId()))
        self.hotkey_registered = ctypes.windll.user32.RegisterHotKey(
            self.hotkey_window, TALK_HOTKEY_ID, MOD_SHIFT | MOD_NOREPEAT, VK_CAPITAL
        )
        self.hotkey_pressed = False
        self.last_hotkey_at = float("-inf")
        self.hotkey_filter = TalkHotkey(self._activate_hotkey)
        if self.hotkey_registered:
            QApplication.instance().installNativeEventFilter(self.hotkey_filter)
        self.talk.setToolTip(
            "Start or stop microphone capture with Shift + Caps Lock"
        )
        self.hotkey_timer = QTimer(self)
        self.hotkey_timer.timeout.connect(self._check_hotkey)
        self.hotkey_timer.start(20)
        self.wake_timer = QTimer(self)
        self.wake_timer.timeout.connect(self._sync_wake_listener)
        self.wake_timer.start(250)
        self.trash_timer = QTimer(self)
        self.trash_timer.timeout.connect(self._expire_trash)
        self.trash_timer.start(60 * 60 * 1000)
        QTimer.singleShot(1000, self._expire_trash)

    # ---------- reactor drive ----------

    def on_level(self, value):
        self.mic_level = max(0.0, min(1.0, float(value)))

    def tick(self):
        blocked = self._playback_blocks_wake()
        level = self.mic_level if self.listening else (
            self.output_level if self.speaking_proc is not None else 0.0
        )
        self.reactor.target = min(1.0, math.sqrt(level) * 1.8)
        if self.wake_button.isChecked():
            label = "Wake: Playback paused" if blocked else "Wake: On"
            if self.wake_button.text() != label:
                self.wake_button.setText(label)

    def _playback_blocks_wake(self):
        output = self.playback_meter.peak() if self.playback_meter is not None else None
        self.output_level = output or 0.0
        now = time.monotonic()
        if output is None or output > 0.0001:
            self.output_quiet_at = now + 1.0
        return now < self.output_quiet_at

    # ---------- controls ----------

    def _set_yolo(self, enabled):
        self.yolo.setText("YOLO: On" if enabled else "Ask for approval")
        self.voice_settings.setValue("yolo", enabled)
        self._log_activity(f"YOLO {'enabled' if enabled else 'disabled'}")
        if enabled and self.approval is not None:
            self._answer_approval("task approved", self.approval)

    def toggle_mode(self):
        anchor = self._ring_position()
        self.mode = {"smart": "task", "task": "chat", "chat": "smart"}[self.mode]
        self.mode_btn.setText(f"Mode: {self.mode.capitalize()}")
        self.task_options.setVisible(self.mode != "chat")
        self.entry.setPlaceholderText({
            "smart": "Ask J.E.A.N. or give it a task…",
            "task": "Tell J.E.A.N. what to do…",
            "chat": "Ask J.E.A.N. a question…",
        }[self.mode])
        self._resize_around_ring(anchor)

    def _log_activity(self, message):
        self.activity_lines.append(f"{time.strftime('%H:%M:%S')}  {message[:1200]}")
        self.activity_lines[:] = self.activity_lines[-200:]
        if hasattr(self, "activity_view"):
            self.activity_view.setPlainText("\n".join(self.activity_lines))
            self.activity_view.verticalScrollBar().setValue(
                self.activity_view.verticalScrollBar().maximum()
            )

    def _show_activity(self):
        if not hasattr(self, "activity_window"):
            self.activity_window = QDialog(self)
            self.activity_window.setWindowTitle("J.E.A.N. activity")
            self.activity_window.resize(650, 380)
            layout = QVBoxLayout(self.activity_window)
            self.activity_view = QPlainTextEdit()
            self.activity_view.setReadOnly(True)
            layout.addWidget(self.activity_view)
        self.activity_view.setPlainText("\n".join(self.activity_lines))
        self.activity_window.show()
        self.activity_window.raise_()

    def _terminate(self):
        self.stop_speech_playback()
        if self.voice_turn is not None:
            self.voice_turn.abandoned = True
            self._stop_capture()
        if self.foreground_cancel is not None:
            self.foreground_cancel.set()
        self._cancel_task()
        self.show_result("Terminate received", "Stopping active J.E.A.N. work.")

    def toggle_backend(self):
        self.backend = ("project9-whisper" if self.backend == "project2-hybrid"
                        else "project2-hybrid")
        self.backend_btn.setText("ASR: Project 9" if self.backend == "project9-whisper"
                                 else "ASR: Project 2")

    def toggle_message(self):
        anchor = self._ring_position()
        visible = self.card.isHidden()
        self.card.setVisible(visible)
        self.toggle.setText("Hide" if visible else "Message")
        self._resize_around_ring(anchor)
        if visible:
            self.entry.setFocus()

    def _ring_position(self):
        return self.reactor.mapToGlobal(self.reactor.rect().center())

    def _resize_around_ring(self, anchor):
        self.layout().invalidate()
        self.layout().activate()
        self.resize(self.sizeHint().expandedTo(self.minimumSizeHint()))
        self.layout().activate()
        if self.isVisible():
            offset = anchor - self._ring_position()
            if not offset.isNull():
                self.move(self.pos() + offset)

    # ---------- voice in ----------

    def _check_hotkey(self):
        key_state = ctypes.windll.user32.GetAsyncKeyState
        pressed = bool(key_state(0x10) & key_state(VK_CAPITAL) & 0x8000)
        if pressed and not self.hotkey_pressed:
            self._activate_hotkey()
        self.hotkey_pressed = pressed

    def _activate_hotkey(self):
        now = time.monotonic()
        if now - self.last_hotkey_at >= 0.3:
            self.last_hotkey_at = now
            self._activate_talk()

    def _activate_talk(self):
        if not self.card.isVisible():
            self.toggle_message()
        self.raise_()
        if self.talk.isEnabled():
            self.toggle_listen()

    def _wake_idle(self):
        return (self.wake_button.isChecked() and self.voice_turn is None
                and not self.busy and self.task_proc is None
                and self.approval is None and self.speaking_proc is None
                and time.monotonic() >= self.output_quiet_at)

    def _can_listen_for_stop(self):
        return (self.wake_button.isChecked() and not self.listening
                and self.speaking_proc is None and time.monotonic() >= self.output_quiet_at)

    def _microphone(self, *, wake=False):
        if self.playback_meter is None:
            raise OSError("Windows could not verify the microphone endpoint.")
        return self.playback_meter.microphone(verify_all_roles=wake)

    def _set_wake_enabled(self, enabled):
        self.wake_button.setText("Wake: On" if enabled else "Wake: Off")
        self._sync_wake_listener()

    def _sync_wake_listener(self):
        if self._playback_blocks_wake() or not self._can_listen_for_stop():
            self._stop_wake_listener()
            return
        terminate_only = not self._wake_idle()
        approval = self.approval if self.voice_turn is None else None
        if approval is not None and approval is self.approval_listener_failed:
            approval = None
        if (self.wake_proc is not None and self.wake_terminate_only == terminate_only
                and self.wake_approval is approval):
            return
        self._stop_wake_listener()
        try:
            self._microphone(wake=True)
        except OSError as exc:
            self.wake_button.setChecked(False)
            self.show_result("Wake listener unavailable", str(exc))
            return
        proc = QProcess(self)
        self.wake_proc = proc
        self.wake_terminate_only = terminate_only
        self.wake_approval = approval
        proc.finished.connect(lambda code, _status: self._wake_finished(proc, code))
        proc.errorOccurred.connect(
            lambda error: self._wake_finished(proc, 1)
            if error == QProcess.ProcessError.FailedToStart else None
        )
        proc.start("powershell.exe", [
            "-NoLogo", "-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass",
            "-File", str(HERE / ("approval-listener.ps1" if approval else "wake-listener.ps1")),
            *(["-TerminateOnly"] if terminate_only and approval is None else []),
        ])

    def _stop_wake_listener(self):
        proc = self.wake_proc
        if proc is not None:
            self.wake_proc = None
            if proc.state() != QProcess.ProcessState.NotRunning:
                proc.kill()
                proc.waitForFinished(500)
            proc.deleteLater()

    def _wake_finished(self, proc, code):
        if self.wake_proc is not proc:
            return
        output = bytes(proc.readAllStandardOutput()).decode(errors="replace").splitlines()
        error = bytes(proc.readAllStandardError()).decode(errors="replace").strip()
        approval = self.wake_approval
        self.wake_proc = None
        proc.deleteLater()
        if approval is not None and ("APPROVE" in output or "DENY" in output):
            if not code and not self._playback_blocks_wake() and self._can_listen_for_stop():
                self._answer_approval("task approved" if "APPROVE" in output else "deny task", approval)
        elif not code and "TERMINATE" in output:
            if not self._playback_blocks_wake() and self._can_listen_for_stop():
                self._terminate()
        elif approval is not None:
            if self.approval is approval:
                self.approval_listener_failed = approval
                approval.setInformativeText("Use Talk, type “task approved”, or click Yes.")
        elif code or "WAKE" not in output:
            self.wake_button.setChecked(False)
            self.show_result("Wake listener unavailable", error or proc.errorString())
        elif not self._playback_blocks_wake() and self._wake_idle():
            self._activate_talk()

    def toggle_listen(self):
        if self.listening:
            self._stop_capture()
        else:
            self._start_capture()

    def _start_capture(self):
        task = self.task_proc if self.cancel_task_button.isEnabled() else None
        if self.voice_turn is not None or (self.busy and task is None and self.approval is None):
            self.bridge.result.emit("JEAN needs a moment",
                                    "Finish the current step before listening.")
            return
        self._stop_wake_listener()
        self.stop_speech_playback()
        try:
            microphone = self._microphone()
            directory = tempfile.TemporaryDirectory(prefix="jean-voice-", ignore_cleanup_errors=True)
        except OSError as exc:
            self.show_result("Voice unavailable", str(exc))
            return
        root = Path(directory.name)
        turn = VoiceTurn(self.mode, self.backend, task, directory, approval=self.approval)
        try:
            argv, cwd = voice_bench.capture_command(
                root / "clip.wav",
                root / "stop.marker",
                max_seconds=20.0, microphone=microphone)
            proc = subprocess.Popen(
                argv, cwd=cwd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                text=True, encoding="utf-8", errors="replace",
                creationflags=NO_WINDOW)
        except (RuntimeError, OSError) as exc:
            directory.cleanup()
            self.show_result("Voice unavailable", str(exc))
            return
        self.voice_turn = turn
        self.capture_proc = proc
        self.listening = True
        self.mic_level = 0.0
        self.talk.setText("Opening mic…")
        self.talk.setEnabled(False)
        self.status.setText("●  OPENING MIC")
        QTimer.singleShot(35_000, lambda: self._expire_capture(turn, proc))
        threading.Thread(target=self._capture_reader, args=(turn, proc), daemon=True).start()

    def _on_capture_ready(self, turn):
        if self.voice_turn is not turn or turn.abandoned:
            return
        self.talk.setText("■ End recording")
        self.talk.setEnabled(True)
        self.status.setText("●  LISTENING")
        self.show_result("Microphone is listening", (
            'Say “task approved” or “deny task”.' if turn.approval is not None
            else 'Say “task status” or “cancel task”.' if turn.task is not None
            else "Speak now, then click End recording."
        ))

    def _stop_capture(self):
        turn = self.voice_turn
        if turn is not None and self.capture_proc is not None and turn.released_at is None:
            turn.released_at = time.monotonic()
            try:
                (Path(turn.directory.name) / "stop.marker").touch()
            except OSError as exc:
                self.show_result("Could not end capture", str(exc))

    def _expire_capture(self, turn, proc):
        if self.voice_turn is turn and self.capture_proc is proc and proc.poll() is None:
            proc.kill()

    def _capture_reader(self, turn, proc):
        saw_ready = False
        error = ""
        diagnostic = ""
        ended_at = 0.0
        try:
            with proc.stdout:
                for line in proc.stdout:
                    line = line.strip()
                    if line.startswith("LEVEL "):
                        try:
                            self.bridge.level.emit(float(line.split(maxsplit=1)[1]))
                        except ValueError:
                            pass
                    elif line == "READY":
                        saw_ready = True
                        self.bridge.capture_ready.emit(turn)
                    elif line == "ENDED":
                        ended_at = time.monotonic()
                    elif line.startswith("ERROR "):
                        error = line[6:]
                    elif line and not line.startswith(("SPEECH ", "DONE ")):
                        diagnostic = line
            proc.wait(timeout=5)
        except (OSError, subprocess.TimeoutExpired) as exc:
            if proc.poll() is None:
                proc.kill()
                proc.wait()
            error = str(exc)
        if not saw_ready or proc.returncode != 0:
            error = error or diagnostic or "microphone capture did not finish successfully"
        self.bridge.capture_finished.emit(turn, error, ended_at)

    def _on_capture_finished(self, turn, error, ended_at):
        if self.voice_turn is not turn:
            turn.directory.cleanup()
            return
        self.listening = False
        self.mic_level = 0.0
        self.capture_proc = None
        self.talk.setText("● Talk")
        self.talk.setEnabled(True)
        clip = Path(turn.directory.name) / "clip.wav"
        if turn.abandoned or error or not clip.is_file():
            if not turn.abandoned:
                self.show_result("Microphone failed", error or "no WAV was produced")
            self._finish_voice(turn)
            return
        start = turn.released_at or ended_at or time.monotonic()
        self.status.setText("●  TRANSCRIBING")
        self.detail.setText(f"{turn.backend} is decoding the clip…")

        def work():
            try:
                outcome = voice_bench.decode_clip(turn.backend, clip, timeout=240)
                if (turn.backend == "project2-hybrid" and not turn.abandoned
                        and "Expected one model under" in (outcome["error"] or "")):
                    turn.asr_fallback = True
                    turn.backend = "project9-whisper"
                    outcome = voice_bench.decode_clip(turn.backend, clip, timeout=240)
                transcript, failure = outcome["transcript"].strip(), outcome["error"] or ""
            except (OSError, RuntimeError, ValueError) as exc:
                transcript, failure = "", str(exc)
            finally:
                turn.directory.cleanup()
            self.bridge.decoded.emit(turn, transcript, failure, round((time.monotonic() - start) * 1000))

        threading.Thread(target=work, daemon=True).start()

    def _on_decoded(self, turn, transcript, error, elapsed_ms):
        if self.voice_turn is not turn or turn.abandoned:
            self._finish_voice(turn)
            return
        if turn.asr_fallback:
            self._log_activity("Project 2 model unavailable. Retried this recording with Project 9.")
            if self.backend == "project2-hybrid":
                self.toggle_backend()
        if error or not transcript:
            self.show_result("Transcription failed", error or "empty transcript")
            self._finish_voice(turn)
            return
        address = WAKE_ADDRESS.match(transcript)
        if address and voice_bench.normalize_tokens(transcript[address.end():]) == "terminate":
            self._finish_voice(turn)
            self._terminate()
            return
        if address:
            transcript = transcript[address.end():].strip()
            if not transcript:
                self._finish_voice(turn)
                if not self._playback_blocks_wake():
                    self._activate_talk()
                return
        self.show_result("Heard you", f"{transcript}\n{turn.backend} · {elapsed_ms} ms to text")
        if self._answer_approval(transcript, turn.approval):
            self._finish_voice(turn)
            return
        mode = request_mode(turn.mode, transcript)
        if turn.task is not None:
            self._voice_task_control(turn, transcript)
        else:
            self._finish_voice(turn)
            self.busy = True
            self._submit_request(mode, transcript)

    def _finish_voice(self, turn):
        turn.directory.cleanup()
        if self.voice_turn is turn:
            self.voice_turn = None
            self._update_status()

    def _voice_task_control(self, turn, transcript):
        words = voice_bench.normalize_tokens(transcript)
        if self.task_proc is not turn.task:
            self.show_result("Task already finished", "Start a new recording for another request.")
        elif words in {"task status", "status", "what is it doing", "what s it doing", "whats it doing"}:
            self._finish_voice(turn)
            self._request_task_status(speak=True, task_id=self.active_task_id)
            return
        elif words in {"cancel task", "cancel the task"}:
            self._finish_voice(turn)
            self._cancel_task()
            return
        else:
            self.show_result("Task still running", 'Say “task status” or “cancel task”.')
        self._finish_voice(turn)

    def _submit_request(self, mode, text):
        desktop_request = voice_bench.normalize_tokens(site_request_text(text))
        if desktop_request in {
            "desktop windows", "list windows", "show open windows",
            "what s on my screen", "what is on my screen", "can you see what s on my screen",
        }:
            self._list_desktop_windows()
            return
        if self._handle_memory(text) or self._handle_workflow(text):
            self._finish_busy_ui()
            return
        scope = clock_request(text)
        if scope:
            self.last_local_step = None
            try:
                answer = clock_answer(scope)
            except (OSError, subprocess.SubprocessError):
                self.show_result("Clock unavailable", "Windows could not read the Sri Lanka clock.")
                self._finish_busy_ui()
                return
            self.show_result("Sri Lanka time" if scope == "sri-lanka" else "Device time", answer)
            self.last_local_step = ["clock", scope]
            self._finish_busy_ui()
            self._speak_async(answer, False)
            return
        mode = request_mode(mode, text)
        if mode == "task":
            self.last_local_step = None
        self._log_activity(f"{mode.capitalize()} request: {text}")
        command, _, task_id = text.partition(" ")
        if command.lower() == "/status":
            if task_id.strip():
                self.task_id.setText(task_id.strip())
            self._request_task_status()
            self._finish_busy_ui()
            return
        if mode == "task" and self._handle_trash(text):
            return
        elif mode == "task" and gmail_request(text):
            self._open_gmail()
            self._finish_busy_ui()
        elif mode == "task" and self._open_requested_site(text):
            self._finish_busy_ui()
        elif mode == "task" and command.lower() == "/browse":
            self._browse(task_id.strip())
        elif mode == "chat":
            self._run_chat(text)
        else:
            self._run_task(text)

    def _finish_busy(self):
        # Safe from a worker thread: the connected GUI slot owns all widgets.
        self.bridge.work_finished.emit()

    def _finish_busy_ui(self):
        self.foreground_cancel = None
        self.busy = False
        self._update_status()

    def _update_status(self):
        if self.listening:
            label = "LISTENING"
        elif self.approval is not None:
            label = "AWAITING APPROVAL"
        elif self.voice_turn is not None:
            label = "TRANSCRIBING"
        elif self.speaking_proc is not None:
            label = "SPEAKING"
        elif self.task_proc is not None:
            label = "CANCELLING" if self.cancel_sent else "TASK"
        elif self.busy:
            label = "CHATTING"
        else:
            label = "READY"
        self.status.setText(f"●  {label}")

    # ---------- chat (read-only) ----------

    def _run_chat(self, text):
        self.status.setText("●  CHATTING")
        self.detail.setText("jean chat is thinking (read-only)…")

        cancel = self.foreground_cancel = threading.Event()

        def work():
            reply, error = self._jean([str(EXE), "chat", text], timeout=180, cancel=cancel)
            if cancel.is_set():
                self._finish_busy()
                return
            if error and PROVIDER_NONE_MARKER in error:
                # Finish busy first so the setup slot sees an idle chat.
                self._finish_busy()
                self.bridge.chat_setup.emit(reply.strip() or error)
                return
            if error:
                self.bridge.result.emit("Chat failed", error)
            elif re.search(r'\brun as["”]? (?:a )?task\b', reply, re.IGNORECASE):
                self.bridge.task_retry.emit(text)
            else:
                self.bridge.chat_setup.emit("")
                self.bridge.result.emit("JEAN", reply.strip() or "(empty reply)")
                self.bridge.transcript.emit(f"JEAN: {reply.strip()}")
                self.bridge.speak_requested.emit(reply.strip(), False)
            self._finish_busy()

        threading.Thread(target=work, daemon=True).start()

    def _offer_task_retry(self, text):
        if not self.entry.text().strip():
            self.entry.setText(text)
        self.show_result("Run this as a task", "Choose Mode: Task, then press Send to retry.")

    def _chat_idle(self):
        return not self.busy and self.voice_turn is None and self.task_proc is None

    def _on_chat_setup(self, diagnostic):
        self.chat_setup_detail = diagnostic
        if diagnostic and self._chat_idle():
            self._present_chat_setup()

    def _present_chat_setup(self):
        self.show_result(CHAT_SETUP_TITLE, CHAT_SETUP_HINT)
        self.detail.setToolTip(self.chat_setup_detail)

    def _choose_workspace(self):
        directory = QFileDialog.getExistingDirectory(self, "Choose task workspace", self.workspace.text())
        if directory:
            self.workspace.setText(directory)

    def _open_requested_site(self, text):
        address = web_request(text)
        remainder = ""
        if address is None:
            parts = re.split(r"\s+and\s+", text, maxsplit=1, flags=re.IGNORECASE)
            if len(parts) != 2 or web_request(parts[0]) != "https://www.youtube.com/":
                requested = SITE_REQUEST.fullmatch(site_request_text(text))
                if requested and requested["browser"]:
                    self.show_result("Which website?", f'I heard “{requested["destination"]}”. '
                                     "Say the site name again or enter its URL.")
                    return True
                return False
            address = "https://www.youtube.com/"
            remainder = "\nPlease repeat the second request separately."
        if not valid_web_url(address):
            self.show_result("Browser request failed", "Enter a valid web URL.")
            return True
        opened = QDesktopServices.openUrl(QUrl(address))
        if opened:
            self.last_local_step = ["browser", address]
            self.last_open_step = self.last_local_step
        self.show_result("Sent to your browser" if opened else "Could not open browser",
                         QUrl(address).toDisplayString() + (remainder if opened else ""))
        return True

    def _open_gmail(self):
        output, error = self._jean([
            "powershell.exe", "-NoProfile", "-NonInteractive", "-Command",
            "Get-StartApps | Where-Object Name -eq 'Gmail' | Select-Object -ExpandProperty AppID",
        ], timeout=5)
        apps = sorted({line.strip() for line in output.splitlines()
                       if re.fullmatch(r"[\w.!-]+", line.strip())})
        remembered = self.voice_settings.value("gmail_app", "")
        app = remembered if remembered in apps else apps[0] if len(apps) == 1 else None
        if error or app is None:
            self.show_result("Could not open Gmail", error or (
                "Windows lists multiple Gmail apps." if apps else "Windows did not list a Gmail app."
            ))
            return False
        try:
            subprocess.Popen(["explorer.exe", rf"shell:AppsFolder\{app}"], creationflags=NO_WINDOW)
        except OSError as exc:
            self.show_result("Could not open Gmail", str(exc))
            return False
        self.voice_settings.setValue("gmail_app", app)
        self.last_local_step = ["gmail", ""]
        self.last_open_step = self.last_local_step
        self.show_result("Sent to Gmail app", "Opening your installed Gmail app.")
        return True

    def _handle_workflow(self, text):
        text = site_request_text(text).rstrip(".").strip()
        if text.casefold() in {"list workflows", "show my workflows"}:
            self.voice_settings.beginGroup("workflows")
            try:
                names = self.voice_settings.childKeys()
            finally:
                self.voice_settings.endGroup()
            self.show_result("Saved workflows", ", ".join(names) or "No workflows saved yet.")
            return True
        teach = WORKFLOW_TEACH.fullmatch(text)
        remember = re.fullmatch(rf"remember this as (?P<name>{WORKFLOW_NAME})", text, re.IGNORECASE)
        command = WORKFLOW_COMMAND.fullmatch(text)
        repeat = text.casefold()
        if not (teach or remember or command or repeat in {"open it again", "run that again", "do that again"}):
            return False
        if not (teach or remember or command):
            step = self.last_open_step if repeat == "open it again" else self.last_local_step
            if step is None:
                self.show_result("Nothing to repeat", "Run a browser, Gmail, or clock action first.")
            else:
                self._run_workflow("Last action", [step])
            return True
        name = (teach or remember or command)["name"].strip().casefold()
        key = f"workflows/{name}"
        revisions = workflow_revisions(self.voice_settings.value(key, "null", type=str))
        if teach or remember:
            steps = ([workflow_step(item.strip()) for item in teach["steps"].split(";")]
                     if teach else [self.last_local_step])
            if not valid_workflow(steps):
                self.show_result("Could not learn workflow",
                                 "Use up to five browser, Gmail, or clock actions separated by semicolons.")
            else:
                if not revisions or revisions[-1] != steps:
                    revisions = [*revisions[-7:], steps]
                self._save_workflow(key, revisions)
                slot = next((value for kind, value in steps if kind == "input"), None)
                suffix = f" with {slot} …" if slot else ""
                self.show_result("Workflow saved", f'Say “run workflow {name}{suffix}” to use it.')
        elif command["input"] and command["verb"].casefold() != "run":
            self.show_result("Workflow command needs no input", name)
        elif command["verb"].casefold() == "forget":
            self.voice_settings.remove(key)
            self.voice_settings.sync()
            self.show_result("Workflow removed", name)
        elif not revisions:
            self.show_result("Workflow unavailable", f'No usable workflow named “{name}”.')
        elif command["verb"].casefold() == "undo":
            if len(revisions) < 2:
                self.show_result("No previous workflow version", name)
            else:
                self._save_workflow(key, revisions[:-1])
                self.show_result("Previous workflow restored", name)
        elif command["verb"].casefold() == "show":
            labels = {"browser": "Open", "clock": "Read clock", "gmail": "Open Gmail",
                      "input": "Use input"}
            summary = "\n".join(f"{labels[kind]} {value}".rstrip() for kind, value in revisions[-1])
            self.show_result("Saved workflow", f"{name} · {len(revisions)} version(s)\n{summary}")
        else:
            try:
                steps = bind_workflow(revisions[-1], command["input"], command["value"])
            except ValueError as exc:
                self.show_result("Workflow needs input", str(exc))
            else:
                self._run_workflow(name, steps)
        return True

    def _save_workflow(self, key, revisions):
        self.voice_settings.setValue(key, json.dumps({"version": 1, "revisions": revisions}))
        self.voice_settings.sync()

    def _handle_memory(self, text):
        text = WAKE_ADDRESS.sub("", text.strip())
        remember = re.fullmatch(r"remember that\s+(.+)", text, re.IGNORECASE)
        search = re.fullmatch(r"(?:recall|what do you remember about)\s+(.+?)\??",
                              text, re.IGNORECASE)
        if remember:
            args = ["add", "--", remember[1]]
        elif search:
            args = ["search", "--limit", "5", "--", search[1]]
        elif text.casefold().rstrip("?.") == "list memories":
            args = ["list", "--limit", "10"]
        else:
            return False
        output, error = self._jean([str(EXE), "memory", *args], timeout=5)
        if error:
            self.show_result("Memory request failed", error)
        else:
            title = "Memory saved" if remember else "Local memory"
            self.show_result(title, output.strip() or "No matching memories.")
        return True

    def _run_workflow(self, name, steps):
        results = []
        for kind, value in steps:
            if kind == "clock":
                try:
                    result = clock_answer(value)
                except (OSError, subprocess.SubprocessError):
                    self.show_result("Workflow stopped", f"{name}: Windows could not read the clock.")
                    return
            elif kind == "gmail":
                if not self._open_gmail():
                    return
                result = "Sent to Gmail app."
            else:
                if not QDesktopServices.openUrl(QUrl(value)):
                    self.show_result("Workflow stopped", f"{name}: browser handoff failed.")
                    return
                result = f"Sent to browser: {value}"
            self.last_local_step = [kind, value]
            if kind != "clock":
                self.last_open_step = self.last_local_step
            results.append(result)
        self.show_result("Workflow ran", f"{name}\n" + "\n".join(results))

    def _expire_trash(self):
        if self.trash.directory.is_dir():
            def work():
                try:
                    self.trash.purge()
                except (OSError, ValueError) as exc:
                    self.bridge.result.emit("Trash cleanup deferred", str(exc))

            threading.Thread(target=work, daemon=True).start()

    def _handle_trash(self, text):
        request = trash_request(text)
        if request is None:
            return False
        action, argument = request
        workspace = Path(self.workspace.text().strip() or ROOT)
        if not workspace.is_absolute():
            workspace = ROOT / workspace

        def work():
            try:
                if action == "list":
                    entries = self.trash.list()
                    self.bridge.result.emit("Workspace trash",
                                            "\n".join(entries) or "Trash is empty.")
                elif action == "put":
                    entry_id = self.trash.put(workspace, argument)
                    self.bridge.result.emit("Moved to workspace trash",
                                            f"{argument}\nRestore with /trash restore {entry_id}")
                elif action == "restore":
                    path = self.trash.restore(argument)
                    self.bridge.result.emit("Restored from trash", str(path))
                else:
                    removed = self.trash.purge(everything=True)
                    self.bridge.result.emit("Workspace trash cleaned", f"Removed {removed} entries.")
            except (OSError, ValueError, KeyError) as exc:
                self.bridge.result.emit("Workspace trash request failed", str(exc))
            finally:
                self._finish_busy()

        threading.Thread(target=work, daemon=True).start()
        return True

    def _browse(self, url):
        if not url or not url.startswith(("https://", "http://")):
            self.show_result("Enter a browser URL", "Example: /browse https://example.com/")
            self._finish_busy_ui()
            return
        self.status.setText("●  BROWSING")
        self.show_result("Opening page", url)
        cancel = self.foreground_cancel = threading.Event()

        def work():
            try:
                output, error = self._jean(
                    [str(EXE), "browser", "run", "--json", "--", url], timeout=75,
                    cancel=cancel,
                )
                if cancel.is_set():
                    return
                try:
                    report, _ = json.JSONDecoder().raw_decode(output.lstrip())
                except ValueError:
                    self.bridge.result.emit("Browser request failed", error or output.strip())
                    return
                if report.get("title"):
                    detail = f'{report["title"]}\n{report["url"]}'
                    if report.get("screenshot"):
                        detail += f'\nScreenshot: {report["screenshot"]}'
                    self.bridge.result.emit("Page opened", detail)
                else:
                    self.bridge.result.emit("Browser request failed", report.get("detail") or error)
            finally:
                self._finish_busy()

        threading.Thread(target=work, daemon=True).start()

    def _run_task(self, text):
        channel = secrets.token_hex(16)
        args = ["desktop-request", "--channel", channel]
        for name, value in (
            ("workspace", self.workspace.text().strip()),
            ("worker", self.worker.text().strip()),
            ("profile", self.profile.currentData()),
            ("instruction", self.instruction.text().strip()),
        ):
            if value:
                args.extend([f"--{name}={value}"])
        self.task_channel = f"JEAN:{channel} "
        self.task_output = []
        self.task_error = ""
        self.review_preview = None
        self.active_task_id = ""
        self.cancel_sent = False
        self.activity.clear()
        self.status.setText("●  TASK")
        self.show_result("Task request", text)
        proc = QProcess(self)
        self.task_proc = proc
        proc.setWorkingDirectory(str(ROOT))
        proc.readyReadStandardOutput.connect(self._read_task_output)
        proc.readyReadStandardError.connect(self._read_task_error)
        if text.split(maxsplit=1)[0] not in ("/review", "/apply", "/discard"):
            proc.started.connect(lambda: self.cancel_task_button.setEnabled(not self.cancel_sent))
        proc.started.connect(
            lambda: self._send_task_input({"kind": "cancel"}) if self.cancel_sent else None
        )
        proc.finished.connect(self._task_finished)
        proc.errorOccurred.connect(self._task_process_error)
        proc.start(str(EXE), [*args, "--", text])

    def _read_task_output(self, final=False):
        proc = self.task_proc
        while proc.canReadLine() or (final and proc.bytesAvailable()):
            data = proc.readLine() if proc.canReadLine() else proc.readAllStandardOutput()
            line = bytes(data).decode("utf-8", errors="replace").rstrip("\r\n")
            if line.startswith(self.task_channel):
                try:
                    event = json.loads(line[len(self.task_channel):])
                except json.JSONDecodeError:
                    continue
                if event["kind"] == "task" and not self.active_task_id:
                    self.active_task_id = event["id"]
                    self.task_id.setText(self.active_task_id)
                    self.activity.setText(self.active_task_id)
                    self._log_activity(f"Task started: {self.active_task_id}")
                    self.poll_task.start()
                elif event["kind"] == "approval":
                    self._ask_approval(event)
                elif event["kind"] == "review":
                    self.review_preview = event
                continue
            if line:
                self._log_activity(line)
                self.task_output.append(line[:4000])
                # Keep the opening result context plus the latest detail.
                if len(self.task_output) > 100:
                    del self.task_output[25]
                if self.approval is None:
                    self.detail.setText(line[:600])

    def _read_task_error(self):
        data = bytes(self.task_proc.readAllStandardError()).decode("utf-8", errors="replace")
        self.task_error = (self.task_error + data)[-4000:]

    def _ask_approval(self, event):
        process = self.task_proc
        if process is None or self.cancel_sent:
            return
        if self.yolo.isChecked():
            self._log_activity(f'YOLO approved {event["operation"]}: {event["summary"]}')
            self._send_task_input({"kind": "approve", "id": event["id"], "allow": True})
            return
        if self.approval is not None:
            self.approval.reject()
        dialog = QMessageBox(self)
        dialog.setWindowTitle("Approve J.E.A.N. action")
        dialog.setModal(False)
        dialog.setTextFormat(Qt.TextFormat.PlainText)
        dialog.setText(event["summary"])
        details = [f'{event["operation"]} · {event["tier"]}']
        if event["tool"]:
            details.append(f'Tool: {event["tool"]}')
        details.extend(event["capabilities"])
        dialog.setInformativeText("\n".join(details) + '\nSay or type “task approved” to continue.')
        dialog.setStandardButtons(QMessageBox.StandardButton.Yes | QMessageBox.StandardButton.No)
        dialog.setDefaultButton(QMessageBox.StandardButton.No)
        dialog.setEscapeButton(QMessageBox.StandardButton.No)
        self.approval = dialog
        self.approval_listener_failed = None
        self._update_status()

        def answer(code):
            if self.approval is dialog:
                self.approval = None
                if self.task_proc is process and not self.cancel_sent:
                    allow = code == QMessageBox.StandardButton.Yes.value
                    self._send_task_input({"kind": "approve", "id": event["id"],
                                           "allow": allow})
                    self.show_result("Action approved" if allow else "Action declined",
                                     event["summary"])
                self._update_status()
            dialog.deleteLater()

        dialog.finished.connect(answer)
        timeout = QTimer(dialog)
        timeout.setSingleShot(True)
        timeout.timeout.connect(dialog.reject)
        timeout.start(min(event["timeout_ms"], 2_147_483_647))
        dialog.show()

    def _answer_approval(self, text, expected):
        words = voice_bench.normalize_tokens(text)
        if words in {"task approved", "approve task", "approve the task", "i approve"}:
            answer = QMessageBox.StandardButton.Yes
        elif words in {"deny task", "deny the task", "task denied"}:
            answer = QMessageBox.StandardButton.No
        elif expected is not None and words in {"yes", "no"}:
            answer = (QMessageBox.StandardButton.Yes if words == "yes"
                      else QMessageBox.StandardButton.No)
        else:
            return False
        if expected is not None and self.approval is expected:
            expected.done(answer.value)
        else:
            self.show_result("No matching approval", "There is no pending action for that reply.")
        return True

    def _send_task_input(self, message):
        if self.task_proc is not None and self.task_proc.state() == QProcess.ProcessState.Running:
            self.task_proc.write((json.dumps(message) + "\n").encode("utf-8"))

    def _cancel_task(self):
        if self.task_proc is None or self.cancel_sent:
            return
        self.cancel_sent = True
        self._send_task_input({"kind": "cancel"})
        if self.approval is not None:
            self.approval.reject()
        self.cancel_task_button.setEnabled(False)
        self.status.setText("●  CANCELLING")

    def _resume_task(self):
        task_id = self.task_id.text().strip()
        if not task_id or self.busy or self.voice_turn is not None:
            return
        self.busy = True
        self._run_task(f"/resume {task_id}")

    def _review_task(self):
        task_id = self.task_id.text().strip()
        if task_id and not self.busy and self.voice_turn is None:
            self.busy = True
            self._run_task(f"/review {task_id}")

    def _show_review(self, review):
        dialog = QDialog(self)
        dialog.setWindowTitle(f"Review {review['id']}")
        dialog.resize(780, 540)
        dialog.setAttribute(Qt.WidgetAttribute.WA_DeleteOnClose)
        layout = QVBoxLayout(dialog)
        metadata = QLabel(
            f"{review['workspace']}\nBaseline {review['baseline']}\n"
            f"SHA-256 {review['sha256']} · {review['state']}\n"
            "Independent verification passed."
        )
        metadata.setTextFormat(Qt.TextFormat.PlainText)
        metadata.setWordWrap(True)
        layout.addWidget(metadata)
        patch = QPlainTextEdit()
        patch.setReadOnly(True)
        patch.setPlainText(review["patch"])
        layout.addWidget(patch)
        buttons = QHBoxLayout()
        buttons.addStretch()
        if review["state"] == "pending":
            for label, request in (
                ("Discard", f"/discard {review['id']}"),
                ("Apply and stage", f"/apply {review['id']} {review['sha256']}"),
            ):
                button = QPushButton(label)
                button.clicked.connect(
                    lambda checked=False, command=request: self._decide_review(dialog, command)
                )
                buttons.addWidget(button)
        close = QPushButton("Close")
        close.clicked.connect(dialog.reject)
        buttons.addWidget(close)
        layout.addLayout(buttons)
        dialog.open()

    def _decide_review(self, dialog, command):
        if not self.busy and self.voice_turn is None:
            dialog.accept()
            self.busy = True
            self._run_task(command)

    def _request_task_status(self, *, speak=False, task_id=None):
        task_id = self.task_id.text().strip() if task_id is None else task_id
        if not task_id:
            if speak:
                self.show_result("Task is starting", "Its task ID is not available yet.")
            return
        if speak:
            self.spoken_status.add(task_id)
        if task_id in self.status_pending:
            return
        self.status_pending.add(task_id)

        def work():
            output, error = self._jean([str(EXE), "task", "--", task_id], timeout=15)
            if error:
                status = error
            else:
                labels = ("state ", "worker ", "model ", "latest step ", "verifier ", "checkpoint ", "resumed as ")
                lines = [line.strip() for line in output.splitlines()
                         if line.strip().startswith(labels)]
                status = "\n".join(lines) or output.strip()
            self.bridge.task_status.emit(task_id, status[:600])

        threading.Thread(target=work, daemon=True).start()

    def _show_task_status(self, task_id, status):
        self.status_pending.discard(task_id)
        if task_id == self.task_id.text().strip() or task_id in self.spoken_status:
            self.activity.setText(status)
        if task_id in self.spoken_status:
            self.spoken_status.discard(task_id)
            self._speak_async(status, True)

    def _task_process_error(self, error):
        if error == QProcess.ProcessError.FailedToStart:
            self.task_error = self.task_proc.errorString()
            self._task_finished(-1, QProcess.ExitStatus.CrashExit)

    def _task_finished(self, code, _exit_status):
        self._read_task_output(final=True)
        self._read_task_error()
        if self.approval is not None:
            self.approval.reject()
        self.poll_task.stop()
        self.cancel_task_button.setEnabled(False)
        self.task_proc.deleteLater()
        self.task_proc = None
        self._finish_busy_ui()
        if code == 0 and self.review_preview is not None:
            self.show_result("Verified patch", "Review the patch before applying or discarding.")
            self._show_review(self.review_preview)
            return
        if self.active_task_id:
            self._request_task_status()
        if code == 0:
            title = "Task completed"
        elif code == 130:
            title = "Task cancelled"
        else:
            title = "Task needs attention"
        output = "\n".join(self.task_output)
        if code and self.task_error.strip():
            output = (output + "\n" + self.task_error.strip()).strip()
        result_lines = [
            line for line in self.task_output
            if line.startswith(("result:", "result ", "verification:", "verdict ", "outcome ", "reason:"))
        ]
        summary = ("\n".join(result_lines[-5:]) or self.task_error.strip()
                   or output[-600:] or f"Exit code {code}")
        self.show_result(title, summary[:600])
        self.detail.setToolTip(output[-8000:])
        self._speak_async(f"{title}. {summary}", True)

    def _list_desktop_windows(self):
        self.show_result("Reading open windows", "Checking the live Windows session with Cua.")
        cancel = self.foreground_cancel = threading.Event()

        def work():
            try:
                output, error = self._jean(
                    [str(EXE), "mcp", "call", "cua", "list_windows"],
                    timeout=20, cancel=cancel,
                    env={**os.environ, "CUA_DRIVER_RS_TELEMETRY_ENABLED": "0"},
                )
                if not cancel.is_set():
                    if error:
                        self.bridge.result.emit("Desktop observation failed", error)
                    else:
                        _, separator, listing = output.partition("\nresult   (")
                        details = listing.partition("\n")[2].strip() if separator else output.strip()
                        self.bridge.result.emit("Open windows", details)
            finally:
                self._finish_busy()

        threading.Thread(target=work, daemon=True).start()

    def _jean(self, argv, timeout, cancel=None, env=None):
        try:
            with subprocess.Popen(argv, cwd=ROOT, stdout=subprocess.PIPE,
                                  stderr=subprocess.PIPE, text=True, encoding="utf-8",
                                  errors="replace", creationflags=NO_WINDOW, env=env) as proc:
                deadline = time.monotonic() + timeout
                while True:
                    remaining = deadline - time.monotonic()
                    if remaining <= 0 or (cancel is not None and cancel.is_set()):
                        proc.kill()
                        proc.communicate()
                        return "", ("terminated" if cancel is not None and cancel.is_set()
                                    else f"timed out after {timeout}s")
                    try:
                        stdout, stderr = proc.communicate(
                            timeout=min(remaining, .25) if cancel is not None else remaining
                        )
                        break
                    except subprocess.TimeoutExpired:
                        continue
        except OSError as exc:
            return "", str(exc)
        out = stdout + (("\n" + stderr) if stderr else "")
        if proc.returncode != 0:
            tail = [l for l in out.strip().splitlines() if l.strip()]
            return out, tail[-1] if tail else f"exit code {proc.returncode}"
        return out, ""

    # ---------- typed chat ----------

    def submit(self):
        text = self.entry.text().strip()
        if not text:
            return
        if voice_bench.normalize_tokens(text) == "jean terminate":
            self.entry.clear()
            self._terminate()
            return
        if self._answer_approval(text, self.approval):
            self.entry.clear()
            return
        command, _, task_id = text.partition(" ")
        if command.lower() == "/status":
            self.entry.clear()
            self._request_task_status(task_id=task_id.strip() or None)
            return
        if self.busy or self.voice_turn is not None:
            return
        self.entry.clear()
        self.busy = True
        self._submit_request(self.mode, text)

    # ---------- spoken output (project 9 TTS + project 12 readback) ----------

    def _load_voices(self):
        path = r"SOFTWARE\Microsoft\Speech\Voices\Tokens"
        try:
            with winreg.OpenKey(winreg.HKEY_LOCAL_MACHINE, path) as tokens:
                for index in range(winreg.QueryInfoKey(tokens)[0]):
                    with winreg.OpenKey(tokens, winreg.EnumKey(tokens, index)) as token:
                        name = winreg.QueryValue(token, None).split(" - ", 1)[0]
                        with winreg.OpenKey(token, "Attributes") as attributes:
                            gender = winreg.QueryValueEx(attributes, "Gender")[0]
                        self.voice_choice.addItem(f"{name} ({gender})", name)
        except OSError:
            pass
        selected = self.voice_choice.findData(self.voice_settings.value("voice", ""))
        self.voice_choice.setCurrentIndex(max(0, selected))
        self.voice_choice.currentIndexChanged.connect(
            lambda: self.voice_settings.setValue("voice", self.voice_choice.currentData())
        )

    def _speak_async(self, text, task_result):
        if not text or self.voice_turn is not None:
            return
        self.stop_speech_playback()
        argv = [str(EXE), "speak"]
        argv.extend(["--rate", str(self.voice_rate.currentData())])
        if self.voice_choice.currentData():
            argv.extend(["--voice", self.voice_choice.currentData()])
        if task_result:
            argv.append("--task-result")
        try:
            proc = subprocess.Popen(
                argv, cwd=ROOT, stdin=subprocess.PIPE, stderr=subprocess.PIPE,
                encoding="utf-8", errors="replace", creationflags=NO_WINDOW)
        except OSError as exc:
            self.bridge.result.emit("Speech unavailable", str(exc))
            return
        self.speaking_proc = proc
        self.stop_speech.setEnabled(True)
        self.status.setText("●  SPEAKING")

        def watch():
            error_text = ""
            try:
                _, stderr = proc.communicate(text, timeout=90)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.communicate()
                error_text = "playback exceeded 90 seconds and was stopped"
            else:
                if proc.returncode != 0:
                    error_text = stderr.strip() or f"exit code {proc.returncode}"
            self.bridge.playback_finished.emit(proc, error_text)

        threading.Thread(target=watch, daemon=True).start()

    def _on_playback_finished(self, proc, error):
        stopped_by_user = proc in self._stopped_speech
        self._stopped_speech.discard(proc)
        if self.speaking_proc is proc:
            self.speaking_proc = None
            self.stop_speech.setEnabled(False)
        if error and not stopped_by_user:
            self.bridge.result.emit("Speech playback failed", error)
        self._update_status()

    def stop_speech_playback(self):
        proc = self.speaking_proc
        if proc is None:
            return
        self._stopped_speech.add(proc)
        if proc.poll() is None:
            proc.terminate()
            QTimer.singleShot(500, lambda: proc.kill() if proc.poll() is None else None)
        self.speaking_proc = None
        self.stop_speech.setEnabled(False)
        self._update_status()

    # ---------- existing overlay behavior ----------

    def closeEvent(self, event):
        if self.voice_turn is not None:
            self.voice_turn.abandoned = True
            self._stop_capture()
            self.show_result("Ending voice input", "Wait for the recording to close before closing J.E.A.N.")
            event.ignore()
            return
        if self.task_proc is not None:
            if self.cancel_task_button.isEnabled():
                self._cancel_task()
            self.show_result("Task in progress", "Wait for the task to finish before closing.")
            event.ignore()
            return
        self.stop_speech_playback()
        if self.foreground_cancel is not None:
            self.foreground_cancel.set()
        self.wake_timer.stop()
        self.hotkey_timer.stop()
        self._stop_wake_listener()
        if self.hotkey_registered:
            QApplication.instance().removeNativeEventFilter(self.hotkey_filter)
            ctypes.windll.user32.UnregisterHotKey(self.hotkey_window, TALK_HOTKEY_ID)
        self.anim.stop()
        if self.playback_meter is not None:
            self.playback_meter.close()
        super().closeEvent(event)

    def show_transcript(self, text):
        if text:
            self.detail.setText(text[:600])

    def refresh(self):
        def work():
            output, error = self._jean([str(EXE), "model", "status"], timeout=15)
            if not error:
                for line in output.splitlines():
                    parts = line.split(None, 1)
                    if parts and parts[0] == "provider":
                        if parts[1].strip() == "none":
                            self.bridge.chat_setup.emit(output.strip())
                        break
            try:
                result = subprocess.run([str(EXE), "status"], cwd=ROOT, capture_output=True,
                                        text=True, encoding="utf-8", errors="replace", timeout=15,
                                        creationflags=NO_WINDOW)
                rows = [line for line in result.stdout.splitlines() if line.startswith("t-")]
                if result.returncode == 0 and rows:
                    self.bridge.latest_task.emit(rows[0].split()[0])
            except (OSError, subprocess.TimeoutExpired):
                # An unavailable activity history does not prevent a new request.
                return
        threading.Thread(target=work, daemon=True).start()

    def _show_latest_task(self, task_id):
        if not self.task_id.text() and not self.busy:
            self.task_id.setText(task_id)

    def show_result(self, title, detail):
        self._log_activity(f"{title}: {detail}")
        self.title.setText(title)
        self.detail.setText(detail)
        self.detail.setToolTip(detail if len(detail) > 180 else "")


if __name__ == "__main__":
    app = QApplication(sys.argv)
    window = Overlay()
    window.toggle_message()
    window.show()
    window.raise_()
    window.activateWindow()
    sys.exit(app.exec())
