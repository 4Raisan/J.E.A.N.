"""Optional audio controls and desktop layout editor."""

import json

from PyQt6.QtCore import QSignalBlocker, Qt, QTimer
from PyQt6.QtWidgets import (
    QAbstractItemView, QDialog, QDialogButtonBox, QGridLayout, QLabel,
    QListWidget, QListWidgetItem, QMenu, QPushButton, QSlider, QVBoxLayout, QWidget,
)

LAYOUT_KEY = "overlay/sections-v1"


def read_layout(settings, sections):
    try:
        rows = json.loads(settings.value(LAYOUT_KEY, "[]", type=str))
        saved = {key: pinned for key, pinned in rows if key in sections and isinstance(pinned, bool)}
    except (ValueError, TypeError):
        saved = {}
    return {**saved, **{key: False for key in sections if key not in saved}}


class LayoutEditor(QDialog):
    def __init__(self, parent, sections, layout):
        super().__init__(parent)
        self.setWindowTitle("Edit J.E.A.N. layout")
        self.resize(350, 300)
        self.sections = sections
        box = QVBoxLayout(self)
        hint = QLabel("Drag sections to reorder them. Check a section to show it when J.E.A.N. starts. All sections stay in More.")
        hint.setWordWrap(True)
        box.addWidget(hint)
        self.rows = QListWidget()
        self.rows.setDragDropMode(QAbstractItemView.DragDropMode.InternalMove)
        self.rows.setDefaultDropAction(Qt.DropAction.MoveAction)
        box.addWidget(self.rows)
        buttons = QDialogButtonBox(QDialogButtonBox.StandardButton.Save
                                  | QDialogButtonBox.StandardButton.Cancel
                                  | QDialogButtonBox.StandardButton.RestoreDefaults)
        buttons.accepted.connect(self.accept)
        buttons.rejected.connect(self.reject)
        buttons.button(QDialogButtonBox.StandardButton.RestoreDefaults).clicked.connect(
            lambda: self.populate(dict.fromkeys(sections, False)))
        box.addWidget(buttons)
        self.populate(layout)

    def populate(self, layout):
        self.rows.clear()
        for key, pinned in layout.items():
            item = QListWidgetItem(self.sections[key])
            item.setData(Qt.ItemDataRole.UserRole, key)
            item.setFlags((item.flags() | Qt.ItemFlag.ItemIsUserCheckable)
                          & ~Qt.ItemFlag.ItemIsDropEnabled)
            item.setCheckState(Qt.CheckState.Checked if pinned else Qt.CheckState.Unchecked)
            self.rows.addItem(item)

    def selection(self):
        return {item.data(Qt.ItemDataRole.UserRole): item.checkState() == Qt.CheckState.Checked
                for item in (self.rows.item(index) for index in range(self.rows.count()))}


class AudioControls(QWidget):
    def __init__(self, meter, open_devices, wake, voice, pace):
        super().__init__()
        self.meter = meter
        self.rows = {}
        grid = QGridLayout(self)
        grid.setContentsMargins(0, 0, 0, 0)
        for index, (capture, title) in enumerate(((True, "Microphone"), (False, "Speaker"))):
            label = QLabel(title)
            slider = QSlider(Qt.Orientation.Horizontal)
            slider.setRange(0, 100)
            slider.setAccessibleName(title + " Windows level")
            slider.valueChanged.connect(lambda value, capture=capture: self._change(capture, value=value))
            mute = QPushButton("Mute")
            mute.setObjectName("mini")
            mute.setCheckable(True)
            mute.setAccessibleName(title + " mute")
            mute.clicked.connect(lambda checked, capture=capture: self._change(capture, muted=checked))
            self.rows[capture] = label, slider, mute
            grid.addWidget(label, index, 0)
            grid.addWidget(slider, index, 1)
            grid.addWidget(mute, index, 2)
        settings = QPushButton("Windows audio devices…")
        settings.setObjectName("mini")
        devices = QMenu(settings)
        devices.addAction("Recording devices", lambda: open_devices(True))
        devices.addAction("Playback devices", lambda: open_devices(False))
        settings.setMenu(devices)
        grid.addWidget(wake, 2, 0)
        grid.addWidget(settings, 2, 1, 1, 2)
        grid.addWidget(voice, 3, 0, 1, 2)
        grid.addWidget(pace, 3, 2)
        self.error = QLabel()
        self.error.setWordWrap(True)
        self.error.hide()
        grid.addWidget(self.error, 4, 0, 1, 3)
        self.poll = QTimer(self)
        self.poll.setInterval(750)
        self.poll.timeout.connect(self.refresh)

    def showEvent(self, event):
        super().showEvent(event)
        self.refresh()
        self.poll.start()

    def hideEvent(self, event):
        self.poll.stop()
        super().hideEvent(event)

    def refresh(self):
        for capture, (label, slider, mute) in self.rows.items():
            title = "Microphone" if capture else "Speaker"
            try:
                if self.meter is None:
                    raise OSError("Windows audio controls are unavailable.")
                level, muted = self.meter.volume_state(capture)
            except OSError as exc:
                label.setText(title)
                label.setToolTip(str(exc))
                slider.setEnabled(False)
                mute.setEnabled(False)
                continue
            slider.setEnabled(True)
            mute.setEnabled(True)
            label.setText(f"{title} {level}%")
            label.setToolTip("Windows default device level")
            with QSignalBlocker(slider), QSignalBlocker(mute):
                if not slider.isSliderDown():
                    slider.setValue(level)
                mute.setChecked(muted)
                mute.setText("Unmute" if muted else "Mute")

    def _change(self, capture, *, value=None, muted=None):
        if self.meter is None:
            return
        try:
            if value is not None:
                self.meter.set_volume(capture, value)
            if muted is not None:
                self.meter.set_muted(capture, muted)
        except OSError as exc:
            self.error.setText(str(exc))
            self.error.show()
        else:
            self.error.hide()
        self.refresh()
