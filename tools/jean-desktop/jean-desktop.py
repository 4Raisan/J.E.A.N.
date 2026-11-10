import math
import subprocess
import sys
import threading
from pathlib import Path
from PyQt6.QtCore import Qt, QTimer, QPoint, QObject, pyqtSignal
from PyQt6.QtGui import QColor, QPainter, QRadialGradient, QPen, QFont
from PyQt6.QtWidgets import QApplication, QWidget, QVBoxLayout, QHBoxLayout, QLabel, QPushButton, QLineEdit, QFrame, QGraphicsDropShadowEffect

ROOT = Path(__file__).resolve().parents[2]
EXE = ROOT / "target" / "debug" / "jean.exe"

class Bridge(QObject):
    result = pyqtSignal(str, str)

class Orb(QWidget):
    def __init__(self):
        super().__init__()
        self.phase = 0.0
        self.setFixedSize(188, 188)
        self.timer = QTimer(self)
        self.timer.timeout.connect(self.animate)
        self.speaking = False

    def animate(self):
        self.phase += 0.055
        self.update()

    def paintEvent(self, event):
        painter = QPainter(self)
        painter.setRenderHint(QPainter.RenderHint.Antialiasing)
        pulse = math.sin(self.phase) * 3 if self.speaking else 0
        center = QPoint(94, 94)
        glow = QRadialGradient(94.0, 94.0, 91.0)
        glow.setColorAt(0, QColor(117, 104, 255, 120))
        glow.setColorAt(0.65, QColor(112, 89, 255, 45))
        glow.setColorAt(1, QColor(112, 89, 255, 0))
        painter.setPen(Qt.PenStyle.NoPen)
        painter.setBrush(glow)
        painter.drawEllipse(center, 91, 91)
        radius = 52 + pulse
        sphere = QRadialGradient(76.0, 68.0, 107.0)
        sphere.setColorAt(0, QColor("#ecddff"))
        sphere.setColorAt(0.24, QColor("#b6a5ff"))
        sphere.setColorAt(0.62, QColor("#7768ed"))
        sphere.setColorAt(1, QColor("#343578"))
        painter.setBrush(sphere)
        painter.setPen(QPen(QColor(220, 212, 255, 115), 1.2))
        painter.drawEllipse(center, int(radius), int(radius))
        painter.setPen(QColor(255, 255, 255, 215))
        painter.setFont(QFont("Segoe UI Symbol", 23))
        painter.drawText(self.rect(), Qt.AlignmentFlag.AlignCenter, "✦")

class Overlay(QWidget):
    def __init__(self):
        super().__init__()
        self.setWindowTitle("JEAN")
        self.setWindowFlags(Qt.WindowType.FramelessWindowHint | Qt.WindowType.WindowStaysOnTopHint | Qt.WindowType.Window)
        self.setAttribute(Qt.WidgetAttribute.WA_TranslucentBackground)
        self.resize(370, 270)
        self.bridge = Bridge()
        self.bridge.result.connect(self.show_result)
        self.drag_point = None
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
        orbrow.addWidget(Orb())
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
        row = QHBoxLayout()
        self.entry = QLineEdit()
        self.entry.setPlaceholderText("Write a message to JEAN…")
        self.entry.returnPressed.connect(self.submit)
        row.addWidget(self.entry)
        send = QPushButton("↑")
        send.setObjectName("send")
        send.setToolTip("Send message")
        send.clicked.connect(self.submit)
        row.addWidget(send)
        box.addLayout(row)
        self.note = QLabel("Conversation setup is still required")
        self.note.setObjectName("note")
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
        """)
        screen = QApplication.primaryScreen().availableGeometry()
        self.move(screen.center().x() - self.width() // 2, screen.center().y() - self.height() // 2)
        self.refresh()

    def toggle_message(self):
        visible = not self.card.isVisible()
        self.card.setVisible(visible)
        self.toggle.setText("Hide" if visible else "Message")
        self.adjustSize()
        if visible:
            self.entry.setFocus()

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

    def submit(self):
        if self.entry.text().strip():
            self.title.setText("Your message is saved here")
            self.detail.setText("Connect a conversation model to send it.")
            self.status.setText("●  SETUP NEEDED")
            self.entry.setFocus()

    def refresh(self):
        def work():
            try:
                result = subprocess.run([str(EXE), "status"], cwd=ROOT, capture_output=True,
                                        text=True, encoding="utf-8", errors="replace", timeout=15,
                                        creationflags=subprocess.CREATE_NO_WINDOW)
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
