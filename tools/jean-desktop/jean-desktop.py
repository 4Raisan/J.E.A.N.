"""JEAN desktop overlay — voice slice.

Circular particle reactor (project 3's shell-of-points idea, painted with
QPainter instead of WebGL) sits at the centre. Real microphone RMS levels
drive it while listening (voice_capture.py), a speech-like envelope drives
it while the project 9 Windows TTS path speaks, and it keeps breathing when
idle.

Voice in: push Talk, speak, push Talk again (or stay silent past the
endpoint). The captured 16 kHz WAV is decoded by the selected backend —
project 9's Faster-Whisper base.en by default, or project 2's
Zipformer+Parakeet hybrid — then routed by mode:

  * Chat  → `jean chat <text>`        (read-only conversation, never a task)
  * Task  → `jean exec --goal <text> <tool> <args>`  (first word = tool id)

Chat replies are spoken verbatim. Long task results are shortened by
the Rust `jean speak --task-result` command before Windows Speech plays them. Stop halts
playback; provider/playback failures are surfaced in the status line.
"""
import math
import subprocess
import sys
import tempfile
import threading
from pathlib import Path

import voice_bench

from PyQt6.QtCore import Qt, QTimer, QPoint, QObject, pyqtSignal
from PyQt6.QtGui import QColor, QPainter, QRadialGradient, QPen, QFont
from PyQt6.QtWidgets import (QApplication, QWidget, QVBoxLayout, QHBoxLayout,
                             QLabel, QPushButton, QLineEdit, QFrame,
                             QGraphicsDropShadowEffect)

ROOT = Path(__file__).resolve().parents[2]
HERE = Path(__file__).resolve().parent
EXE = ROOT / "target" / "debug" / "jean.exe"
NO_WINDOW = getattr(subprocess, "CREATE_NO_WINDOW", 0)


class Bridge(QObject):
    result = pyqtSignal(str, str)      # title, detail
    transcript = pyqtSignal(str)       # heard / reply / task text
    level = pyqtSignal(float)          # measured mic level 0..1
    capture_finished = pyqtSignal(bool, int, str)
    decoded = pyqtSignal(str, str, str, str)  # mode, backend, text, error
    speak_requested = pyqtSignal(str, bool)  # text, task result
    work_finished = pyqtSignal()
    playback_finished = pyqtSignal(object, str)  # process, error text
    refresh_requested = pyqtSignal()


class Reactor(QWidget):
    """Project 3's circular particle reactor, painted in 2D.

    A shell of points orbits a glowing core. Loudness pushes the shell
    outward and brightens it; an idle breath keeps it alive at level zero —
    the same contract as Scene.tsx's `want = max(level, breathe)`.
    """

    COUNT = 340

    def __init__(self):
        super().__init__()
        self.phase = 0.0
        self.level = 0.0
        self.target = 0.0
        self.setFixedSize(200, 200)
        import random
        rng = random.Random(20261111)
        self.particles = [
            (rng.random() * math.tau,                    # base angle
             rng.random(),                              # shell position 0..1
             0.05 + rng.random() * 0.05,                # orbit rate
             rng.random(),                              # seed for jitter
             1.0 + rng.random() * 1.6)                  # point size
            for _ in range(self.COUNT)
        ]
        self.timer = QTimer(self)
        self.timer.timeout.connect(self.animate)
        self.timer.start(33)

    def animate(self):
        self.phase += 0.055
        breathe = (math.sin(self.phase * 0.9) * 0.5 + 0.5) * 0.12
        want = max(min(1.0, self.target), breathe)
        self.level += (want - self.level) * 0.18
        self.update()

    def paintEvent(self, event):
        painter = QPainter(self)
        painter.setRenderHint(QPainter.RenderHint.Antialiasing)
        painter.setPen(Qt.PenStyle.NoPen)
        cx = cy = 100.0
        level = self.level

        # Particle shell — project 3's drifting dust, projected flat.
        tint = min(1.0, level * 1.4)  # violet -> cyan as loudness rises
        for angle0, rad01, rate, seed, size in self.particles:
            angle = angle0 + self.phase * rate * (0.5 + level * 1.6)
            push = 1.0 + level * 0.38 + math.sin(self.phase * 2.0 + seed * 12.0) * 0.02
            radius = (30.0 + rad01 * 58.0) * push
            x = cx + math.cos(angle) * radius
            y = cy + math.sin(angle) * radius * 0.92  # slight ellipse = depth
            alpha = int(90 * (0.30 + level * 0.45) * (1.0 - rad01 * 0.55))
            if alpha <= 4:
                continue
            r = int(min(4, size * (0.8 + level * 0.9)))
            red = int(158 + (110 - 158) * tint)
            green = int(140 + (227 - 140) * tint)
            blue = int(255 + (255 - 255) * tint)
            painter.setBrush(QColor(red, green, blue, alpha))
            painter.drawEllipse(QPoint(int(x), int(y)), r, r)

        # Reactor ring — the "circular" read of project 3's complete ring.
        ring_r = 76.0 + level * 10.0 + math.sin(self.phase * 0.7) * 2.0
        painter.setPen(QPen(QColor(140, 220, 255, int(60 + level * 120)), 1.4))
        painter.setBrush(Qt.BrushStyle.NoBrush)
        painter.drawEllipse(QPoint(int(cx), int(cy)), int(ring_r), int(ring_r * 0.92))

        # Core glow + sphere, as before but breathing with level.
        pulse = math.sin(self.phase) * (2.0 + level * 5.0)
        glow = QRadialGradient(cx, cy, 92.0)
        glow.setColorAt(0, QColor(117, 104, 255, 120 + int(level * 60)))
        glow.setColorAt(0.65, QColor(112, 89, 255, 45))
        glow.setColorAt(1, QColor(112, 89, 255, 0))
        painter.setPen(Qt.PenStyle.NoPen)
        painter.setBrush(glow)
        painter.drawEllipse(QPoint(int(cx), int(cy)), 92, 92)
        radius = 46.0 + pulse
        sphere = QRadialGradient(cx - 18.0, cy - 26.0, 107.0)
        sphere.setColorAt(0, QColor("#ecddff"))
        sphere.setColorAt(0.24, QColor("#b6a5ff"))
        sphere.setColorAt(0.62, QColor("#7768ed"))
        sphere.setColorAt(1, QColor("#343578"))
        painter.setBrush(sphere)
        painter.setPen(QPen(QColor(220, 212, 255, 115), 1.2))
        painter.drawEllipse(QPoint(int(cx), int(cy)), int(radius), int(radius))
        painter.setPen(QColor(255, 255, 255, 215))
        painter.setFont(QFont("Segoe UI Symbol", 21))
        painter.drawText(self.rect(), Qt.AlignmentFlag.AlignCenter, "✦")


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
        self.bridge.capture_finished.connect(self._on_capture_finished)
        self.bridge.decoded.connect(self._on_decoded)
        self.bridge.speak_requested.connect(self._speak_async)
        self.bridge.work_finished.connect(self._finish_busy_ui)
        self.bridge.playback_finished.connect(self._on_playback_finished)
        self.bridge.refresh_requested.connect(self.refresh)
        self.drag_point = None

        # Voice state
        self.mode = "chat"          # "chat" | "task"
        self.backend = "project9-whisper"
        self.listening = False
        self.capture_proc = None
        self.capture_tmp = None
        self.mic_level = 0.0
        self.speaking_proc = None
        self._stopped_speech = set()
        self.busy = False
        self._anim_t = 0.0

        outer = QVBoxLayout(self)
        outer.setContentsMargins(16, 0, 16, 14)
        outer.setSpacing(0)
        header = QHBoxLayout()
        header.addStretch()
        close = QPushButton("×")
        close.setObjectName("close")
        close.setToolTip("Close JEAN")
        close.clicked.connect(self.close)
        header.addWidget(close)
        outer.addLayout(header)
        orbrow = QHBoxLayout()
        orbrow.addStretch()
        self.reactor = Reactor()
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
        self.detail = QLabel("Checking your latest activity…")
        self.detail.setObjectName("detail")
        self.detail.setWordWrap(True)
        box.addWidget(self.detail)
        controls = QHBoxLayout()
        controls.setSpacing(6)
        self.talk = QPushButton("● Talk")
        self.talk.setObjectName("mini")
        self.talk.setToolTip("Start or stop microphone capture")
        self.talk.clicked.connect(self.toggle_listen)
        controls.addWidget(self.talk)
        self.mode_btn = QPushButton("Mode: Chat")
        self.mode_btn.setObjectName("mini")
        self.mode_btn.setToolTip("Chat uses jean chat (read-only); Task parses '<tool.id> <args>' for jean exec")
        self.mode_btn.clicked.connect(self.toggle_mode)
        controls.addWidget(self.mode_btn)
        self.backend_btn = QPushButton("ASR: Project 9")
        self.backend_btn.setObjectName("mini")
        self.backend_btn.setToolTip("Voice decode backend: project 2 Zipformer+Parakeet hybrid (default) or project 9 faster-whisper base.en")
        self.backend_btn.clicked.connect(self.toggle_backend)
        controls.addWidget(self.backend_btn)
        self.stop_speech = QPushButton("Stop")
        self.stop_speech.setObjectName("mini")
        self.stop_speech.setToolTip("Stop spoken playback")
        self.stop_speech.clicked.connect(self.stop_speech_playback)
        self.stop_speech.setEnabled(False)
        controls.addWidget(self.stop_speech)
        box.addLayout(controls)
        row = QHBoxLayout()
        self.entry = QLineEdit()
        self.entry.setPlaceholderText("Write a message to JEAN…")
        self.entry.returnPressed.connect(self.submit)
        row.addWidget(self.entry)
        send = QPushButton("↑")
        send.setObjectName("send")
        send.setToolTip("Send message (chat, read-only)")
        send.clicked.connect(self.submit)
        row.addWidget(send)
        box.addLayout(row)
        self.note = QLabel("Chat = jean chat (read-only) · Task = jean exec · TTS = project 9 Windows speech")
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
        outer.addSpacing(6)
        footer = QLabel("DRAG TO MOVE   ·   ESC TO CLOSE")
        footer.setObjectName("footer")
        footer.setAlignment(Qt.AlignmentFlag.AlignCenter)
        outer.addWidget(footer)
        self.setStyleSheet("""
            #card { background: rgba(24, 28, 44, 242); border: 1px solid rgba(183, 178, 242, 48); border-radius: 22px; }
            QLabel { background: transparent; color: #f5f4ff; border: none; }
            #badge { color: #c8bfff; font-size: 11px; font-weight: 700; letter-spacing: 2px; }
            #status { color: #91dfc5; font-size: 10px; font-weight: 700; }
            #title { font-size: 17px; font-weight: 700; }
            #detail { color: #adb5cd; font-size: 12px; }
            #note { color: #8f9ab5; font-size: 10px; }
            #footer { color: rgba(205, 207, 232, 155); font-size: 9px; letter-spacing: 1px; }
            QLineEdit { color: #f8f7ff; background: rgba(255,255,255,14); border: 1px solid rgba(205,200,255,38); border-radius: 14px; padding: 10px 12px; font-size: 12px; }
            QLineEdit:focus { border: 1px solid #a79aff; }
            #toggle { background: rgba(24,28,44,230); color: #e7e2ff; border: 1px solid rgba(205,200,255,50); border-radius: 15px; padding: 7px 18px; font-size: 11px; }
            #toggle:hover { background: #403b68; }
            #send { background: #a99aff; color: #17152c; border: none; border-radius: 15px; font-size: 21px; font-weight: bold; min-width: 38px; min-height: 38px; }
            #send:hover { background: #c3b9ff; }
            #close { background: rgba(30,32,51,190); color: #e8e5ff; border: 1px solid rgba(255,255,255,30); border-radius: 14px; font-size: 19px; min-width: 28px; min-height: 28px; }
            #close:hover { background: #5d587f; }
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

    # ---------- reactor drive ----------

    def on_level(self, value):
        self.mic_level = max(0.0, min(1.0, float(value) * 1.15))

    def tick(self):
        self._anim_t += 0.05
        t = self._anim_t
        if self.listening:
            target = self.mic_level
        elif self.speaking_proc is not None and self.speaking_proc.poll() is None:
            # Speech-like envelope. Project 3 measures real output levels with a
            # Web Audio analyser; System.Speech gives us no tap, so this is an
            # animated stand-in — documented, not measured.
            target = 0.38 + 0.22 * abs(math.sin(t * 9.1)) + 0.14 * abs(math.sin(t * 23.7))
        elif self.busy:
            target = 0.16 + 0.07 * math.sin(t * 3.0)
        else:
            target = 0.0
        self.reactor.target = target
        if self.speaking_proc is not None and self.speaking_proc.poll() is not None:
            self.speaking_proc = None
            self.stop_speech.setEnabled(False)

    # ---------- controls ----------

    def toggle_mode(self):
        self.mode = "task" if self.mode == "chat" else "chat"
        self.mode_btn.setText(f"Mode: {self.mode.capitalize()}")

    def toggle_backend(self):
        self.backend = ("project9-whisper" if self.backend == "project2-hybrid"
                        else "project2-hybrid")
        self.backend_btn.setText("ASR: Project 9" if self.backend == "project9-whisper"
                                 else "ASR: Project 2")

    def toggle_message(self):
        visible = not self.card.isVisible()
        self.card.setVisible(visible)
        self.toggle.setText("Hide" if visible else "Message")
        self.adjustSize()
        if visible:
            self.entry.setFocus()

    # ---------- voice in ----------

    def toggle_listen(self):
        if self.listening:
            self._stop_capture()
        else:
            self._start_capture()

    def _start_capture(self):
        if self.busy or self.speaking_proc is not None:
            self.bridge.result.emit("JEAN needs a moment",
                                    "Finish the current step before listening.")
            return
        self.capture_tmp = Path(tempfile.mkdtemp(prefix="jean-voice-"))
        try:
            argv, cwd = voice_bench.capture_command(
                self.capture_tmp / "clip.wav",
                self.capture_tmp / "stop.marker",
                max_seconds=20.0)
        except RuntimeError as exc:
            self.capture_tmp = None
            self.bridge.result.emit("Voice unavailable", str(exc))
            return
        try:
            self.capture_proc = subprocess.Popen(
                argv, cwd=cwd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                text=True, encoding="utf-8", errors="replace",
                creationflags=NO_WINDOW)
        except OSError as exc:
            self.capture_tmp = None
            self.bridge.result.emit("Voice unavailable", str(exc))
            return
        self.listening = True
        self.mic_level = 0.0
        self.talk.setText("■ Stop")
        self.status.setText("●  LISTENING")
        threading.Thread(target=self._capture_reader, daemon=True).start()

    def _stop_capture(self):
        if self.capture_tmp is not None:
            (self.capture_tmp / "stop.marker").write_text("stop", encoding="utf-8")

    def _capture_reader(self):
        proc = self.capture_proc
        wav_path = self.capture_tmp / "clip.wav" if self.capture_tmp else None
        saw_ready = False
        try:
            for line in proc.stdout:
                line = line.strip()
                if line.startswith("LEVEL "):
                    try:
                        self.bridge.level.emit(float(line.split()[1]))
                    except ValueError:
                        pass
                elif line == "READY":
                    saw_ready = True
                elif line.startswith("ERROR "):
                    self.bridge.result.emit("Microphone failed", line[6:])
                elif line.startswith("DONE "):
                    pass
            proc.wait(timeout=30)
        except (OSError, subprocess.TimeoutExpired):
            pass
        rc = proc.returncode if proc.returncode is not None else -1
        self.bridge.capture_finished.emit(
            saw_ready, rc, str(wav_path) if wav_path is not None else "")

    def _on_capture_finished(self, saw_ready, returncode, wav_path):
        """Finish capture on the GUI thread before routing the recorded WAV."""
        self.listening = False
        self.mic_level = 0.0
        self.capture_proc = None
        self.talk.setText("● Talk")
        if not saw_ready:
            self.bridge.result.emit("Microphone failed",
                                    "capture never opened an audio stream")
            return
        clip = Path(wav_path) if wav_path else None
        if returncode != 0 or clip is None or not clip.is_file():
            self.bridge.result.emit("Microphone failed",
                                    "no WAV was produced; nothing to decode")
            return
        self._decode_and_route(clip)

    # ---------- decode + route ----------

    def _decode_and_route(self, wav_path):
        self.busy = True
        backend = self.backend
        mode = self.mode
        self.status.setText("●  TRANSCRIBING")
        self.detail.setText(f"{backend} is decoding the clip…")

        def work():
            outcome = voice_bench.decode_clip(backend, wav_path, timeout=240)
            transcript = outcome["transcript"].strip()
            self.bridge.decoded.emit(
                mode, backend, transcript, outcome["error"] or "")

        threading.Thread(target=work, daemon=True).start()

    def _on_decoded(self, mode, backend, transcript, error):
        """Route decoder results on the GUI thread; chat never executes tasks."""
        if error or not transcript:
            self.bridge.result.emit("Transcription failed", error or "empty transcript")
            self.bridge.transcript.emit("")
            self._finish_busy()
            return
        self.bridge.transcript.emit(f"Heard ({backend}): {transcript}")
        if mode == "chat":
            self._run_chat(transcript)
        else:
            self._run_task(transcript)

    def _finish_busy(self):
        # Safe from a worker thread: the connected GUI slot owns all widgets.
        self.bridge.work_finished.emit()

    def _finish_busy_ui(self):
        self.busy = False
        if not self.listening and self.speaking_proc is None:
            self.status.setText("●  READY")

    # ---------- chat (read-only) ----------

    def _run_chat(self, text):
        self.status.setText("●  CHATTING")
        self.detail.setText("jean chat is thinking (read-only)…")

        def work():
            reply, error = self._jean([str(EXE), "chat", text], timeout=180)
            if error:
                self.bridge.result.emit("Chat failed", error)
            else:
                self.bridge.result.emit("JEAN", reply.strip() or "(empty reply)")
                self.bridge.transcript.emit(f"JEAN: {reply.strip()}")
                self.bridge.speak_requested.emit(reply.strip(), False)
            self._finish_busy()

        threading.Thread(target=work, daemon=True).start()

    # ---------- task (jean exec) ----------

    def _run_task(self, text):
        parts = text.split()
        if not parts:
            self.bridge.result.emit("Task needs a tool",
                                    "Task mode expects '<tool.id> <args>', e.g. 'git.status'")
            self._finish_busy()
            return
        tool, args = parts[0], parts[1:]
        argv = [str(EXE), "exec", "--goal", text, tool, *args]
        self.status.setText(f"●  TASK · {tool}")
        self.detail.setText("jean exec is running the tool…")

        def work():
            out, err = self._jean(argv, timeout=600)
            if err:
                self.bridge.result.emit(f"Task {tool} failed", err)
                self._finish_busy()
                return
            shown = out.strip() or "(no output)"
            self.bridge.result.emit(f"Task · {tool}", shown[:600])
            self.bridge.transcript.emit(f"Task result: {shown[:400]}")
            # This is the successful JEAN CLI tool output. The voice surface
            # does not relabel it as independently verified task work.
            self.bridge.speak_requested.emit(shown, True)
            self.bridge.refresh_requested.emit()
            self._finish_busy()

        threading.Thread(target=work, daemon=True).start()

    def _jean(self, argv, timeout):
        try:
            proc = subprocess.run(argv, cwd=ROOT, capture_output=True, text=True,
                                  encoding="utf-8", errors="replace",
                                  timeout=timeout, creationflags=NO_WINDOW)
        except subprocess.TimeoutExpired:
            return "", f"timed out after {timeout}s"
        except OSError as exc:
            return "", str(exc)
        out = (proc.stdout or "") + (("\n" + proc.stderr) if proc.stderr else "")
        if proc.returncode != 0:
            tail = [l for l in out.strip().splitlines() if l.strip()]
            return out, tail[-1] if tail else f"exit code {proc.returncode}"
        return out, ""

    # ---------- typed chat ----------

    def submit(self):
        text = self.entry.text().strip()
        if not text or self.busy:
            return
        self.entry.clear()
        self.busy = True
        self.status.setText("●  CHATTING")
        self.detail.setText("jean chat is thinking (read-only)…")

        def work():
            reply, error = self._jean([str(EXE), "chat", text], timeout=180)
            if error:
                self.bridge.result.emit("Chat failed", error)
            else:
                self.bridge.result.emit("JEAN", reply.strip() or "(empty reply)")
                self.bridge.transcript.emit(f"JEAN: {reply.strip()}")
                self.bridge.speak_requested.emit(reply.strip(), False)
            self._finish_busy()

        threading.Thread(target=work, daemon=True).start()

    # ---------- spoken output (project 9 TTS + project 12 readback) ----------

    def _speak_async(self, text, task_result):
        if not text:
            return
        self.stop_speech_playback()
        argv = [str(EXE), "speak"]
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
        stopped_by_user = proc.pid in self._stopped_speech
        self._stopped_speech.discard(proc.pid)
        if self.speaking_proc is proc:
            self.speaking_proc = None
            self.stop_speech.setEnabled(False)
        if error and not stopped_by_user:
            self.bridge.result.emit("Speech playback failed", error)
        if not self.listening and not self.busy and self.speaking_proc is None:
            self.status.setText("●  READY")

    def stop_speech_playback(self):
        proc = self.speaking_proc
        if proc is None:
            return
        self._stopped_speech.add(proc.pid)
        proc.terminate()
        try:
            proc.wait(timeout=3)
        except subprocess.TimeoutExpired:
            proc.kill()
        self.speaking_proc = None
        self.stop_speech.setEnabled(False)
        if not self.listening and not self.busy:
            self.status.setText("●  READY")

    # ---------- existing overlay behavior ----------

    def mousePressEvent(self, event):
        if event.button() == Qt.MouseButton.LeftButton:
            self.drag_point = event.globalPosition().toPoint() - self.frameGeometry().topLeft()

    def mouseMoveEvent(self, event):
        if self.drag_point is not None and event.buttons() & Qt.MouseButton.LeftButton:
            self.move(event.globalPosition().toPoint() - self.drag_point)

    def mouseReleaseEvent(self, event):
        self.drag_point = None

    def keyPressEvent(self, event):
        if event.key() == Qt.Key.Key_Escape:
            self.close()

    def closeEvent(self, event):
        self._stop_capture()
        if self.capture_proc is not None:
            self.capture_proc.terminate()
        self.stop_speech_playback()
        super().closeEvent(event)

    def show_transcript(self, text):
        if text:
            self.detail.setText(text[:600])

    def refresh(self):
        def work():
            try:
                result = subprocess.run([str(EXE), "status"], cwd=ROOT, capture_output=True,
                                        text=True, encoding="utf-8", errors="replace", timeout=15,
                                        creationflags=NO_WINDOW)
                rows = [line for line in result.stdout.splitlines() if line.startswith("t-")]
                if result.returncode:
                    self.bridge.result.emit("JEAN needs attention", (result.stderr or "Could not load activity").strip())
                elif rows:
                    parts = rows[0].split()
                    self.bridge.result.emit("Latest task · " + parts[1], " ".join(parts[2:4]))
                else:
                    self.bridge.result.emit("Here when you need me", "No recent tasks")
            except (OSError, subprocess.TimeoutExpired) as error:
                self.bridge.result.emit("JEAN needs attention", str(error))
        threading.Thread(target=work, daemon=True).start()

    def show_result(self, title, detail):
        self.title.setText(title)
        self.detail.setText(detail)


if __name__ == "__main__":
    app = QApplication(sys.argv)
    window = Overlay()
    window.show()
    window.raise_()
    window.activateWindow()
    sys.exit(app.exec())
