"""Optional audio controls and desktop layout editor."""

import json
import threading

from PyQt6.QtCore import QSignalBlocker, Qt, QTimer, pyqtSignal
from PyQt6.QtWidgets import (
    QAbstractItemView, QCheckBox, QComboBox, QDialog, QDialogButtonBox, QFormLayout,
    QGridLayout, QHBoxLayout, QLabel, QLineEdit, QListWidget, QListWidgetItem, QMenu,
    QPlainTextEdit, QPushButton, QSlider, QTabWidget, QVBoxLayout, QWidget,
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
    def __init__(self, meter, open_devices, wake, voice, pace, switch_device, test_speaker=None):
        super().__init__()
        self.meter = meter
        self.rows = {}
        self.selectors = {}
        self.device_lists = {}
        self._refresh_ticks = 0
        self.switch_device = switch_device
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
            selector = QComboBox()
            selector.setMinimumContentsLength(12)
            selector.setSizeAdjustPolicy(QComboBox.SizeAdjustPolicy.AdjustToMinimumContentsLengthWithIcon)
            selector.setAccessibleName(title + " device")
            selector.setToolTip("Changes the Windows default and communications device")
            selector.activated.connect(lambda _, capture=capture: self._select(capture))
            self.selectors[capture] = selector
            grid.addWidget(label, index * 2, 0)
            grid.addWidget(selector, index * 2, 1, 1, 2)
            grid.addWidget(slider, index * 2 + 1, 0, 1, 2)
            grid.addWidget(mute, index * 2 + 1, 2)
        settings = QPushButton("Windows audio devices…")
        settings.setObjectName("mini")
        devices = QMenu(settings)
        devices.addAction("Recording devices", lambda: open_devices(True))
        devices.addAction("Playback devices", lambda: open_devices(False))
        settings.setMenu(devices)
        grid.addWidget(wake, 4, 0)
        grid.addWidget(settings, 4, 1, 1, 2)
        grid.addWidget(voice, 5, 0, 1, 2)
        grid.addWidget(pace, 5, 2)
        self.error = QLabel()
        self.error.setWordWrap(True)
        self.error.hide()
        if test_speaker is not None:
            test = QPushButton("Test speaker")
            test.setObjectName("mini")
            test.clicked.connect(test_speaker)
            grid.addWidget(test, 6, 0, 1, 3)
        grid.addWidget(self.error, 7, 0, 1, 3)
        self.poll = QTimer(self)
        self.poll.setInterval(750)
        self.poll.timeout.connect(self.refresh)

    def showEvent(self, event):
        super().showEvent(event)
        self._refresh_ticks = 0
        self.refresh(force_devices=True)
        self.poll.start()

    def hideEvent(self, event):
        self.poll.stop()
        super().hideEvent(event)

    def refresh(self, *, force_devices=False):
        refresh_devices = force_devices or (self._refresh_ticks % 4 == 0)
        self._refresh_ticks += 1
        for capture, (label, slider, mute) in self.rows.items():
            if refresh_devices:
                self._refresh_selector(capture)
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

    def _refresh_selector(self, capture):
        selector = self.selectors[capture]
        if selector.view().isVisible():
            return
        try:
            devices = self.meter.endpoints(capture) if self.meter else []
            selected = self.meter.default_endpoint(capture) if devices else None
        except OSError:
            devices, selected = [], None
        with QSignalBlocker(selector):
            if self.device_lists.get(capture) != devices:
                selector.clear()
                selector.setPlaceholderText("Choose a device" if devices else "No audio devices")
                for endpoint, name in devices:
                    selector.addItem(name, endpoint)
                self.device_lists[capture] = devices
            selector.setCurrentIndex(selector.findData(selected))
            selector.setEnabled(bool(devices))

    def _select(self, capture):
        endpoint = self.selectors[capture].currentData()
        if endpoint is None:
            return
        try:
            self.switch_device(capture, endpoint)
        except OSError as exc:
            self.error.setText(str(exc))
            self.error.show()
        else:
            self.error.hide()
        self.refresh(force_devices=True)

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


class SettingsDialog(QDialog):
    """Unified, clean dark-themed Settings dialog so the main overlay card stays simple
    while everyday customization, safety visibility, and memory controls stay one click away."""

    status_ready = pyqtSignal(str)
    memory_ready = pyqtSignal(str)

    def __init__(self, overlay):
        super().__init__(overlay)
        self.overlay = overlay
        self._bg_cancel = threading.Event()
        self.setWindowTitle("J.E.A.N. Settings")
        self.resize(560, 440)
        self.setStyleSheet("""
            QDialog { background: #141827; color: #f1f0ff; }
            QTabWidget::pane { border: 1px solid rgba(183, 178, 242, 42); border-radius: 10px;
                               background: #181c2c; padding: 6px; }
            QTabBar::tab { background: #1d2236; color: #b9b5e8; padding: 7px 14px;
                           border-top-left-radius: 8px; border-top-right-radius: 8px;
                           margin-right: 4px; font-size: 11px; }
            QTabBar::tab:selected { background: #2d2952; color: #ffffff; font-weight: 600; }
            QLabel { color: #e8e6ff; font-size: 12px; }
            QCheckBox { color: #e8e6ff; spacing: 8px; font-size: 12px; }
            QLineEdit, QComboBox, QPlainTextEdit {
                background: #101320; color: #f6f5ff; border: 1px solid rgba(205, 200, 255, 42);
                border-radius: 8px; padding: 6px 8px; font-size: 12px;
            }
            QPushButton { background: #262a42; color: #e9e6ff; border: 1px solid rgba(205, 200, 255, 48);
                          border-radius: 8px; padding: 6px 12px; font-size: 11px; }
            QPushButton:hover { background: #3b3563; }
        """)

        root = QVBoxLayout(self)
        root.setContentsMargins(14, 14, 14, 14)
        root.setSpacing(10)

        self.tabs = QTabWidget()
        root.addWidget(self.tabs, 1)

        # --- Tab 1: General ---
        general = QWidget()
        gen_form = QFormLayout(general)
        gen_form.setSpacing(10)

        self.mode_box = QComboBox()
        for value, label in (("smart", "Smart (auto Chat or Task)"),
                             ("task", "Task (always run actions)"),
                             ("chat", "Chat (read-only conversation)")):
            self.mode_box.addItem(label, value)
        self.mode_box.currentIndexChanged.connect(
            lambda: self.overlay.set_mode(self.mode_box.currentData())
        )
        gen_form.addRow("Default mode", self.mode_box)

        ws_row = QHBoxLayout()
        self.ws_edit = QLineEdit()
        self.ws_edit.setPlaceholderText("No workspace selected (optional)")
        self.ws_edit.editingFinished.connect(
            lambda: self.overlay.set_workspace_path(self.ws_edit.text().strip())
        )
        ws_browse = QPushButton("Browse…")
        ws_browse.clicked.connect(self._browse_workspace)
        ws_clear = QPushButton("Clear")
        ws_clear.clicked.connect(lambda: self._set_ws(""))
        ws_row.addWidget(self.ws_edit, 1)
        ws_row.addWidget(ws_browse)
        ws_row.addWidget(ws_clear)
        gen_form.addRow("Workspace", ws_row)

        self.speak_check = QCheckBox("Speak replies aloud")
        self.speak_check.toggled.connect(self.overlay.set_speak_replies)
        gen_form.addRow("", self.speak_check)

        self.brief_check = QCheckBox("Keep spoken replies brief (say 'read the full reply' for all)")
        self.brief_check.toggled.connect(self.overlay.set_brief_speech)
        gen_form.addRow("", self.brief_check)

        self.ontop_check = QCheckBox("Always keep J.E.A.N. above other windows")
        self.ontop_check.toggled.connect(self.overlay.set_always_on_top)
        gen_form.addRow("", self.ontop_check)

        self.dream_check = QCheckBox("Consolidate memory automatically when idle")
        self.dream_check.toggled.connect(self.overlay.set_idle_dream_enabled)
        gen_form.addRow("", self.dream_check)

        pos_row = QHBoxLayout()
        center_btn = QPushButton("Center overlay on screen")
        center_btn.clicked.connect(self.overlay.center_on_screen)
        layout_btn = QPushButton("Edit pinned card sections…")
        layout_btn.clicked.connect(self.overlay._edit_layout)
        pos_row.addWidget(center_btn)
        pos_row.addWidget(layout_btn)
        pos_row.addStretch()
        gen_form.addRow("Overlay", pos_row)

        self.tabs.addTab(general, "General")

        # --- Tab 2: Voice & Audio ---
        voice_tab = QWidget()
        voice_form = QFormLayout(voice_tab)
        voice_form.setSpacing(10)

        self.wake_check = QCheckBox("Enable local wake word while idle (“Jean”)")
        self.wake_check.toggled.connect(self.overlay.wake_button.setChecked)
        voice_form.addRow("Wake word", self.wake_check)

        self.voice_box = QComboBox()
        self.voice_box.currentIndexChanged.connect(self._sync_voice_to_overlay)
        voice_form.addRow("Spoken voice", self.voice_box)

        self.pace_box = QComboBox()
        for label, rate in (("Slower pace", -2), ("Natural pace", 0), ("Faster pace", 2)):
            self.pace_box.addItem(label, rate)
        self.pace_box.currentIndexChanged.connect(self._sync_pace_to_overlay)
        voice_form.addRow("Speaking pace", self.pace_box)

        self.listener_label = QLabel()
        self.listener_label.setWordWrap(True)
        voice_form.addRow("Listening engine", self.listener_label)

        audio_btns = QHBoxLayout()
        test_spk = QPushButton("Test speaker")
        test_spk.clicked.connect(self.overlay._test_speaker)
        models_btn = QPushButton("Models && Listening…")
        models_btn.clicked.connect(self.overlay.show_model_settings)
        win_audio = QPushButton("Windows audio…")
        win_audio_menu = QMenu(win_audio)
        win_audio_menu.addAction("Recording devices", lambda: self.overlay._open_audio_devices(True))
        win_audio_menu.addAction("Playback devices", lambda: self.overlay._open_audio_devices(False))
        win_audio.setMenu(win_audio_menu)
        audio_btns.addWidget(test_spk)
        audio_btns.addWidget(models_btn)
        audio_btns.addWidget(win_audio)
        audio_btns.addStretch()
        voice_form.addRow("Tools", audio_btns)

        self.tabs.addTab(voice_tab, "Voice && Audio")

        # --- Tab 3: Safety & Tools ---
        safety_tab = QWidget()
        safety_box = QVBoxLayout(safety_tab)
        safety_box.setSpacing(8)

        self.yolo_check = QCheckBox("Auto-approve routine steps (YOLO mode)")
        self.yolo_check.toggled.connect(self.overlay.yolo.setChecked)
        safety_box.addWidget(self.yolo_check)

        safety_note = QLabel(
            "Workspace file sharing, desktop UI observations, and unlisted browser origins "
            "(such as YouTube or Bing) always ask for explicit approval even when YOLO is on."
        )
        safety_note.setWordWrap(True)
        safety_note.setStyleSheet("color: #9da6c4; font-size: 11px;")
        safety_box.addWidget(safety_note)

        self.status_view = QPlainTextEdit()
        self.status_view.setReadOnly(True)
        self.status_view.setPlaceholderText("Loading subsystem status…")
        safety_box.addWidget(self.status_view, 1)

        safety_btns = QHBoxLayout()
        refresh_status = QPushButton("Refresh status")
        refresh_status.clicked.connect(self.refresh_status_async)
        activity_btn = QPushButton("Activity log")
        activity_btn.clicked.connect(self.overlay._show_activity)
        config_btn = QPushButton("Open config folder")
        config_btn.clicked.connect(self.overlay.open_config_folder)
        safety_btns.addWidget(refresh_status)
        safety_btns.addWidget(activity_btn)
        safety_btns.addWidget(config_btn)
        safety_btns.addStretch()
        safety_box.addLayout(safety_btns)

        self.tabs.addTab(safety_tab, "Safety && Tools")

        # --- Tab 4: Memory & Playbooks ---
        mem_tab = QWidget()
        mem_box = QVBoxLayout(mem_tab)
        mem_box.setSpacing(8)

        mem_hint = QLabel("Local facts J.E.A.N. remembers and verified playbooks for known tasks:")
        mem_hint.setWordWrap(True)
        mem_box.addWidget(mem_hint)

        self.memory_view = QPlainTextEdit()
        self.memory_view.setReadOnly(True)
        self.memory_view.setPlaceholderText("Loading local memory and playbooks…")
        mem_box.addWidget(self.memory_view, 1)

        mem_btns = QHBoxLayout()
        remember_btn = QPushButton("Remember fact…")
        remember_btn.clicked.connect(self.overlay.prompt_remember_fact)
        dream_btn = QPushButton("Dream now")
        dream_btn.clicked.connect(self.overlay._trigger_dream_menu)
        refresh_mem = QPushButton("Refresh")
        refresh_mem.clicked.connect(self.refresh_memory_async)
        mem_btns.addWidget(remember_btn)
        mem_btns.addWidget(dream_btn)
        mem_btns.addWidget(refresh_mem)
        mem_btns.addStretch()
        mem_box.addLayout(mem_btns)

        self.tabs.addTab(mem_tab, "Memory && Playbooks")

        # Footer
        footer = QHBoxLayout()
        footer.addStretch()
        close_btn = QPushButton("Close")
        close_btn.clicked.connect(self.accept)
        footer.addWidget(close_btn)
        root.addLayout(footer)

        self.status_ready.connect(self.status_view.setPlainText)
        self.memory_ready.connect(self.memory_view.setPlainText)
        self.sync_from_overlay()

    def showEvent(self, event):
        super().showEvent(event)
        self._bg_cancel = threading.Event()
        self.sync_from_overlay()
        self.refresh_status_async()
        self.refresh_memory_async()

    def hideEvent(self, event):
        self._bg_cancel.set()
        super().hideEvent(event)

    def closeEvent(self, event):
        self._bg_cancel.set()
        super().closeEvent(event)

    def sync_from_overlay(self):
        ov = self.overlay
        with QSignalBlocker(self.mode_box):
            self.mode_box.setCurrentIndex(max(0, self.mode_box.findData(ov.mode)))
        with QSignalBlocker(self.ws_edit):
            self.ws_edit.setText(ov.workspace.text().strip())
        with QSignalBlocker(self.speak_check):
            self.speak_check.setChecked(ov.speak_replies)
        with QSignalBlocker(self.brief_check):
            self.brief_check.setChecked(ov.brief_speech)
            self.brief_check.setEnabled(ov.speak_replies)
        with QSignalBlocker(self.ontop_check):
            self.ontop_check.setChecked(ov.always_on_top)
        with QSignalBlocker(self.dream_check):
            self.dream_check.setChecked(ov.idle_dream_enabled)
        with QSignalBlocker(self.wake_check):
            self.wake_check.setChecked(ov.wake_button.isChecked())
        with QSignalBlocker(self.yolo_check):
            self.yolo_check.setChecked(ov.yolo.isChecked())
        with QSignalBlocker(self.voice_box):
            self.voice_box.clear()
            for idx in range(ov.voice_choice.count()):
                self.voice_box.addItem(ov.voice_choice.itemText(idx), ov.voice_choice.itemData(idx))
            self.voice_box.setCurrentIndex(max(0, ov.voice_choice.currentIndex()))
        with QSignalBlocker(self.pace_box):
            self.pace_box.setCurrentIndex(max(0, self.pace_box.findData(ov.voice_rate.currentData())))
        self.listener_label.setText(ov.listening_summary())

    def _browse_workspace(self):
        self.overlay._choose_workspace()
        with QSignalBlocker(self.ws_edit):
            self.ws_edit.setText(self.overlay.workspace.text().strip())

    def _set_ws(self, path):
        self.overlay.set_workspace_path(path)
        with QSignalBlocker(self.ws_edit):
            self.ws_edit.setText(path)

    def _sync_voice_to_overlay(self):
        idx = self.voice_box.currentIndex()
        if 0 <= idx < self.overlay.voice_choice.count():
            self.overlay.voice_choice.setCurrentIndex(idx)

    def _sync_pace_to_overlay(self):
        rate = self.pace_box.currentData()
        idx = self.overlay.voice_rate.findData(rate)
        if idx >= 0:
            self.overlay.voice_rate.setCurrentIndex(idx)

    def refresh_status_async(self):
        self.status_view.setPlainText("Checking J.E.A.N. subsystems…")
        cancel = self._bg_cancel

        def emit_safe(text):
            if cancel.is_set():
                return
            try:
                self.status_ready.emit(text)
            except RuntimeError:
                pass

        self.overlay.load_settings_status(emit_safe, cancel=cancel)

    def refresh_memory_async(self):
        self.memory_view.setPlainText("Loading local memories and playbooks…")
        cancel = self._bg_cancel

        def emit_safe(text):
            if cancel.is_set():
                return
            try:
                self.memory_ready.emit(text)
            except RuntimeError:
                pass

        self.overlay.load_settings_memory(emit_safe, cancel=cancel)
