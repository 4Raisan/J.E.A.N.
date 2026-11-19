"""Focused regression tests for tools/jean-desktop (Items 1, 5, 7, 15, 16)."""

import ctypes
import errno
from contextlib import contextmanager
import importlib.util
import json
import os
from pathlib import Path
import queue
import stat
import subprocess
import sys
import tempfile
import threading
import unittest
from unittest import mock

os.environ.setdefault("QT_QPA_PLATFORM", "offscreen")

HERE = Path(__file__).resolve().parent
if str(HERE) not in sys.path:
    sys.path.insert(0, str(HERE))

import audio_meter
import desktop_panels
import gemini_listening
import model_settings
import voice_bench
import workspace_trash

from PyQt6.QtWidgets import QApplication, QMessageBox


def load_jean_desktop():
    spec = importlib.util.spec_from_file_location("jean_desktop_mod", HERE / "jean-desktop.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


JEAN_DESKTOP = load_jean_desktop()


class FakeGeminiConnection:
    """Fake BidiGenerateContent websocket that emits chunks before and after activityEnd."""

    def __init__(self, pre_end_chunks, post_end_chunks, *, send_turn_complete=True):
        self.pre_end_chunks = list(pre_end_chunks)
        self.post_end_chunks = list(post_end_chunks)
        self.send_turn_complete = send_turn_complete
        self.sent = []
        self.incoming = queue.Queue()
        self.activity_ended = False

    def __enter__(self):
        return self

    def __exit__(self, exc_type, exc, tb):
        return False

    def send(self, payload):
        message = json.loads(payload)
        self.sent.append(message)
        if "setup" in message:
            self.incoming.put(json.dumps({"setupComplete": {}}))
        elif "realtimeInput" in message:
            rt = message["realtimeInput"]
            if "audio" in rt and self.pre_end_chunks:
                chunk = self.pre_end_chunks.pop(0)
                self.incoming.put(json.dumps({
                    "serverContent": {"inputTranscription": {"text": chunk}},
                }))
            elif "activityEnd" in rt:
                self.activity_ended = True
                while self.pre_end_chunks:
                    chunk = self.pre_end_chunks.pop(0)
                    self.incoming.put(json.dumps({
                        "serverContent": {"inputTranscription": {"text": chunk}},
                    }))
                for chunk in self.post_end_chunks:
                    self.incoming.put(json.dumps({
                        "serverContent": {"inputTranscription": {"text": chunk}},
                    }))
                if self.send_turn_complete:
                    self.incoming.put(json.dumps({
                        "serverContent": {"turnComplete": True},
                    }))

    def recv(self, timeout=None):
        try:
            return self.incoming.get(timeout=timeout or 0.01)
        except queue.Empty as exc:
            raise TimeoutError("timed out") from exc


class DesktopRegressionTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.app = QApplication.instance() or QApplication([])

    def test_1_gemini_live_preserves_chunks_before_and_after_activity_end(self):
        fake = FakeGeminiConnection(
            pre_end_chunks=["Jean,", "open "],
            post_end_chunks=["Spotify", " and play jazz"],
        )
        live = gemini_listening.Listening(
            gemini_listening.MODEL,
            "AIza-dummy-test-key",
            connect=lambda *args, **kwargs: fake,
        )
        live.chunks.put("AAAA")
        live.chunks.put("BBBB")
        live.ended.set()
        transcript = live.result.result(timeout=5)
        self.assertEqual(transcript, "Jean, open Spotify and play jazz")
        self.assertTrue(fake.activity_ended)

        # Also verify when all chunks arrive before activityEnd and turnComplete arrives after.
        fake_pre_only = FakeGeminiConnection(
            pre_end_chunks=["Open PowerShell"],
            post_end_chunks=[],
        )
        live_pre = gemini_listening.Listening(
            gemini_listening.MODEL,
            "AIza-dummy-test-key",
            connect=lambda *args, **kwargs: fake_pre_only,
        )
        live_pre.chunks.put("AAAA")
        live_pre.ended.set()
        self.assertEqual(live_pre.result.result(timeout=5), "Open PowerShell")

    def test_gemini_transcribe_finalizes_without_turn_complete(self):
        fake = FakeGeminiConnection([], ["Open", " YouTube"], send_turn_complete=False)
        live = gemini_listening.Listening(
            gemini_listening.MODEL, "dummy", connect=lambda *args, **kwargs: fake,
        )
        live.ended.set()
        self.assertEqual(live.result.result(timeout=2), "Open YouTube")

    def test_5_delete_request_routing_strips_wake_address_and_requires_file_or_folder(self):
        self.assertEqual(
            JEAN_DESKTOP.trash_request("Jean, delete file notes.txt"),
            ("put", "notes.txt"),
        )
        self.assertEqual(
            JEAN_DESKTOP.trash_request("Hey Jean, please delete the folder old_drafts"),
            ("put", "old_drafts"),
        )
        self.assertEqual(
            JEAN_DESKTOP.trash_request("Jean, empty the trash"),
            ("clean", ""),
        )
        self.assertIsNone(
            JEAN_DESKTOP.trash_request("delete the unused function in app.py")
        )
        self.assertEqual(
            JEAN_DESKTOP.request_mode("smart", "Jean, delete file notes.txt"),
            "task",
        )
        self.assertEqual(
            JEAN_DESKTOP.request_mode("smart", "delete the unused function in app.py"),
            "task",
        )

        overlay = JEAN_DESKTOP.Overlay()
        try:
            with (
                mock.patch.object(overlay, "_handle_trash", wraps=overlay._handle_trash) as trash_spy,
                mock.patch.object(overlay.trash, "put", return_value="a" * 32) as put_mock,
                mock.patch.object(overlay, "_run_task") as run_task_mock,
            ):
                overlay._submit_request("smart", "Jean, delete file notes.txt")
                self.assertEqual(trash_spy.call_count, 1)
                run_task_mock.assert_not_called()

                overlay._submit_request("smart", "delete the unused function in app.py")
                run_task_mock.assert_called_once_with("delete the unused function in app.py")
        finally:
            overlay.close()

    def test_7_audio_selectors_capture_form_factors_unmeterable_output_and_capture_launcher(self):
        # Built-in (4), Bluetooth (3, 5, 6, 10), and USB (2, 4, 5, 10) inputs are accepted.
        for form, name in (
            (4, "Microphone Array (Realtek(R) Audio)"),
            (5, "Headset (WH-1000XM5 Hands-Free AG Audio)"),
            (6, "Handset (Bluetooth Hands-Free)"),
            (3, "Headphones (Bluetooth Audio Input)"),
            (2, "Line (Focusrite USB Audio)"),
            (10, "Microphone (USB PnP Audio Device)"),
        ):
            self.assertTrue(
                audio_meter.is_capture_input(19, form, name),
                f"Expected form={form} ({name}) to be accepted as a capture input",
            )

        # Output/digital form factors and loopback/mix virtual devices are rejected.
        for form, name in (
            (1, "Speakers (Realtek(R) Audio)"),
            (8, "SPDIF Interface"),
            (9, "HDMI Display Audio"),
            (10, "Stereo Mix (Realtek(R) Audio)"),
            (2, "What U Hear (Sound Blaster)"),
            (4, "Wave Out Mix"),
            (10, "CABLE Output Loopback"),
        ):
            self.assertFalse(
                audio_meter.is_capture_input(19, form, name),
                f"Expected form={form} ({name}) to be rejected",
            )

        # Verify AudioControls selector population with built-in, Bluetooth, and USB inputs.
        fake_meter = mock.MagicMock()
        capture_devices = [
            ("ep-builtin", "Microphone Array (Realtek(R) Audio)"),
            ("ep-bt", "Headset (Bluetooth Hands-Free)"),
            ("ep-usb", "Microphone (USB PnP Sound Device)"),
        ]
        render_devices = [
            ("ep-spk", "Speakers (Realtek(R) Audio)"),
            ("ep-hdmi", "HDMI (Display Audio)"),
        ]
        fake_meter.endpoints.side_effect = lambda cap: capture_devices if cap else render_devices
        fake_meter.default_endpoint.side_effect = lambda cap: "ep-usb" if cap else "ep-spk"
        fake_meter.volume_state.return_value = (75, False)

        from PyQt6.QtWidgets import QComboBox, QPushButton
        controls = desktop_panels.AudioControls(
            fake_meter,
            lambda cap: None,
            QPushButton("Wake"),
            QComboBox(),
            QComboBox(),
            lambda cap, ep: None,
        )
        controls.refresh()
        mic_combo = controls.selectors[True]
        self.assertEqual(
            [mic_combo.itemText(i) for i in range(mic_combo.count())],
            [name for _, name in capture_devices],
        )
        self.assertEqual(mic_combo.currentData(), "ep-usb")

        # Verify PlaybackMeter peak() handling when one output is unmeterable:
        # 1) Useful positive reading from another meterable output is preserved.
        # 2) When meterable outputs read 0.0 and an output is unmeterable, peak() returns None
        #    (never treating unknown playback as silence).
        meter = audio_meter.PlaybackMeter.__new__(audio_meter.PlaybackMeter)
        meter.refresh_at = float("inf")
        meter.meters = ["m1"]
        meter.complete = False  # One output failed Activate(IID_METER)

        with mock.patch.object(audio_meter, "method") as method_mock:
            def fake_read_positive(_m, out_ptr):
                ctypes.cast(out_ptr, ctypes.POINTER(ctypes.c_float))[0] = 0.42
                return 0

            method_mock.return_value = fake_read_positive
            self.assertAlmostEqual(meter.peak(), 0.42, places=5)

            def fake_read_zero(_m, out_ptr):
                ctypes.cast(out_ptr, ctypes.POINTER(ctypes.c_float))[0] = 0.0
                return 0

            method_mock.return_value = fake_read_zero
            self.assertIsNone(meter.peak())

            meter.complete = True
            self.assertEqual(meter.peak(), 0.0)

        # Verify PlaybackMeter.microphone() with built-in, Bluetooth, USB, loopback, and role mismatch.
        def check_mic_with_roles(role_specs, *, verify_all_roles):
            m = audio_meter.PlaybackMeter.__new__(audio_meter.PlaybackMeter)
            m.enumerator = 1
            m.ole = mock.MagicMock()

            def fake_method(interface, slot, result, *params):
                ptr = getattr(interface, "value", interface)
                if ptr == 1 and slot == 4:
                    def get_default(_enum, _flow, role, dev_ptr):
                        if role not in role_specs:
                            return -1
                        ctypes.cast(dev_ptr, ctypes.POINTER(audio_meter.POINTER))[0] = 100 + role
                        return 0
                    return get_default
                if ptr in (100, 101, 102) and slot == 4:
                    def open_store(dev, _mode, store_ptr):
                        dev_val = getattr(dev, "value", dev)
                        ctypes.cast(store_ptr, ctypes.POINTER(audio_meter.POINTER))[0] = dev_val + 100
                        return 0
                    return open_store
                if ptr in (200, 201, 202) and slot == 5:
                    role = ptr - 200
                    form_val, dev_name = role_specs[role]
                    buf = ctypes.create_unicode_buffer(dev_name)
                    open_buffers.append(buf)

                    def read_prop(_store, key_ptr, val_ptr):
                        key = ctypes.cast(key_ptr, ctypes.POINTER(audio_meter.PropertyKey))[0]
                        val = ctypes.cast(val_ptr, ctypes.POINTER(audio_meter.PropertyValue))[0]
                        if key.pid == audio_meter.FRIENDLY_NAME.pid:
                            val.vt = 31
                            val.value = ctypes.addressof(buf)
                        else:
                            val.vt = 19
                            val.value = form_val
                        ctypes.cast(val_ptr, ctypes.POINTER(audio_meter.PropertyValue))[0] = val
                        return 0
                    return read_prop
                return lambda *_a: 0

            open_buffers = []
            with (
                mock.patch.object(audio_meter, "method", side_effect=fake_method),
                mock.patch.object(audio_meter, "release", lambda _p: None),
            ):
                return m.microphone(verify_all_roles=verify_all_roles)

        for form_val, label in (
            (4, "Microphone Array (Built-in)"),
            (5, "Headset (Bluetooth Hands-Free)"),
            (2, "Line (USB Audio Interface)"),
            (10, "USB Condenser Microphone"),
        ):
            self.assertEqual(
                check_mic_with_roles({0: (form_val, label), 1: (form_val, label), 2: (form_val, label)}, verify_all_roles=True),
                label,
            )

        with self.assertRaisesRegex(OSError, "Set a microphone or headset mic"):
            check_mic_with_roles({1: (10, "Stereo Mix (Realtek)")}, verify_all_roles=False)

        with self.assertRaisesRegex(OSError, "Set a microphone or headset mic"):
            check_mic_with_roles({1: (1, "Speakers (Realtek)")}, verify_all_roles=False)

        with self.assertRaisesRegex(OSError, "Default Communication Device"):
            check_mic_with_roles(
                {
                    0: (4, "Microphone Array"),
                    1: (4, "Microphone Array"),
                    2: (5, "Headset (Bluetooth)"),
                },
                verify_all_roles=True,
            )

        # Check sounddevice using the exact Python launcher JEAN uses for voice_capture.py.
        argv, cwd = voice_bench.capture_command(
            HERE / "dummy.wav", HERE / "dummy.stop"
        )
        launcher_prefix = argv[: argv.index("python") + 1]
        proc = subprocess.run(
            [*launcher_prefix, "-c", "import sounddevice, numpy; print(sounddevice.__version__)"],
            cwd=cwd,
            capture_output=True,
            text=True,
            timeout=15,
            check=True,
        )
        self.assertEqual(proc.stdout.strip(), "0.5.6")

    def test_15_overlay_approvals_off_thread_calls_and_credential_isolation(self):
        overlay = JEAN_DESKTOP.Overlay()
        try:
            # 1) Browser and window-list operations route through _run_task (the desktop approval bridge).
            with mock.patch.object(overlay, "_run_task") as run_task_mock:
                overlay._browse("https://example.com/")
                run_task_mock.assert_called_once_with("/browse https://example.com/")
                run_task_mock.reset_mock()

                overlay._list_desktop_windows()
                run_task_mock.assert_called_once_with(
                    "/windows",
                    extra_env={"CUA_DRIVER_RS_TELEMETRY_ENABLED": "0"},
                )

            # 2) Test the overlay approval dialog and response bridging.
            sent_inputs = []
            overlay.task_proc = object()
            overlay.cancel_sent = False
            overlay.yolo.setChecked(False)
            with mock.patch.object(overlay, "_send_task_input", side_effect=sent_inputs.append):
                overlay._ask_approval({
                    "id": 7,
                    "summary": "Open https://example.com/ in controlled browser",
                    "operation": "drive_browser",
                    "tool": "browser",
                    "tier": "normal",
                    "capabilities": ["browser_automation"],
                    "timeout_ms": 30000,
                })
                self.assertIsNotNone(overlay.approval)
                self.assertIn("AWAITING APPROVAL", overlay.status.text())
                overlay._answer_approval("task approved", overlay.approval)
                self.app.processEvents()
                self.assertEqual(
                    sent_inputs,
                    [{"kind": "approve", "id": 7, "allow": True}],
                )

            # 3) Inspect child environment for app discovery using dummy keys and verify off-thread execution.
            overlay.model_profiles = {
                "main": {
                    **model_settings.DEFAULTS["main"],
                    "provider": "openai_compatible",
                    "api_key_env": "CUSTOM_MAIN_API_KEY",
                },
                "decision": {
                    **model_settings.DEFAULTS["decision"],
                    "mode": "shadow",
                    "api_key_env": "CUSTOM_DECISION_API_KEY",
                },
                "listening": model_settings.DEFAULTS["listening"].copy(),
            }
            main_thread = threading.current_thread()
            captured_calls = []
            done_event = threading.Event()

            def fake_jean(argv, timeout, cancel=None, env=None, use_saved_models=True):
                captured_calls.append({
                    "argv": argv,
                    "thread": threading.current_thread(),
                    "env": env,
                    "use_saved_models": use_saved_models,
                })
                done_event.set()
                return "Gmail.AppID\n", ""

            with (
                mock.patch.dict(os.environ, {
                    model_settings.MAIN_KEY: "sk-dummy-main-12345",
                    model_settings.DECISION_KEY: "sk-dummy-decision-12345",
                    "JEAN_MODEL_API_KEY": "sk-dummy-default-main",
                    "JEAN_DECISION_API_KEY": "sk-dummy-default-decision",
                    "CUSTOM_MAIN_API_KEY": "sk-dummy-custom-main",
                    "CUSTOM_DECISION_API_KEY": "sk-dummy-custom-decision",
                }),
                mock.patch.object(overlay, "_jean", side_effect=fake_jean),
                mock.patch.object(JEAN_DESKTOP.subprocess, "Popen") as popen_mock,
            ):
                overlay._open_gmail()
                self.assertTrue(done_event.wait(timeout=3))
                self.app.processEvents()
                self.assertEqual(len(captured_calls), 1)
                call = captured_calls[0]
                self.assertIsNot(call["thread"], main_thread)
                self.assertFalse(call["use_saved_models"])
                for forbidden in (
                    model_settings.MAIN_KEY,
                    model_settings.DECISION_KEY,
                    "JEAN_MODEL_API_KEY",
                    "JEAN_DECISION_API_KEY",
                    "CUSTOM_MAIN_API_KEY",
                    "CUSTOM_DECISION_API_KEY",
                ):
                    self.assertNotIn(forbidden, call["env"])
                    for value in call["env"].values():
                        self.assertNotIn("sk-dummy-", str(value))

                # Also check that _handle_memory runs off the Qt thread.
                captured_calls.clear()
                done_event.clear()
                self.assertTrue(overlay._handle_memory("list memories"))
                self.assertTrue(done_event.wait(timeout=3))
                self.assertIsNot(captured_calls[0]["thread"], main_thread)
                self.assertFalse(captured_calls[0]["use_saved_models"])
        finally:
            overlay.close()

    def test_16_workspace_trash_reliability(self):
        with tempfile.TemporaryDirectory(prefix="jean-trash-test-") as tmp:
            base = Path(tmp)
            ws = base / "workspace"
            ws.mkdir()

            # 1) Non-canonical data path (contains '..').
            non_canonical = base / "nested" / ".." / "trash-store"
            trash = workspace_trash.WorkspaceTrash(non_canonical)

            # 2) Simulated cross-drive move (EXDEV / winerror 17) during put and restore.
            disposable = ws / "subdir" / "deep" / "note.txt"
            disposable.parent.mkdir(parents=True)
            disposable.write_text("recoverable data", encoding="utf-8")

            def exdev_rename(self_path, target):
                err = OSError(errno.EXDEV, "Cross-device link")
                err.winerror = 17
                raise err

            with mock.patch.object(Path, "rename", exdev_rename):
                entry_id = trash.put(ws, "subdir/deep/note.txt")
            self.assertFalse(disposable.exists())
            self.assertEqual(len(trash.list()), 1)

            # 3) Missing restore parents: remove subdir/deep and subdir before restoring.
            (ws / "subdir" / "deep").rmdir()
            (ws / "subdir").rmdir()

            with mock.patch.object(Path, "rename", exdev_rename):
                restored = trash.restore(entry_id)
            self.assertEqual(restored, disposable.resolve())
            self.assertEqual(disposable.read_text(encoding="utf-8"), "recoverable data")

            # 4) Failed cross-drive copy during put preserves original workspace file and leaves no trash entry.
            fail_file = ws / "keep_me.txt"
            fail_file.write_text("must survive failed copy", encoding="utf-8")
            with (
                mock.patch.object(Path, "rename", exdev_rename),
                mock.patch.object(workspace_trash.shutil, "copy2", side_effect=OSError("disk full")),
            ):
                with self.assertRaises(OSError):
                    trash.put(ws, "keep_me.txt")
            self.assertTrue(fail_file.exists())
            self.assertEqual(fail_file.read_text(encoding="utf-8"), "must survive failed copy")
            self.assertEqual(trash.list(), [])

            # 5) Occupied restore path & failed copy during restore both preserve the trash entry.
            entry_id = trash.put(ws, "keep_me.txt")
            fail_file.write_text("new occupant", encoding="utf-8")
            with self.assertRaises(FileExistsError):
                trash.restore(entry_id)
            self.assertEqual(fail_file.read_text(encoding="utf-8"), "new occupant")
            self.assertEqual(len(trash.list()), 1)

            fail_file.unlink()
            with (
                mock.patch.object(Path, "rename", exdev_rename),
                mock.patch.object(workspace_trash.shutil, "copy2", side_effect=OSError("copy interrupted")),
            ):
                with self.assertRaises(OSError):
                    trash.restore(entry_id)
            self.assertFalse(fail_file.exists())
            self.assertEqual(len(trash.list()), 1)
            # Now restore succeeds cleanly.
            trash.restore(entry_id)
            self.assertEqual(fail_file.read_text(encoding="utf-8"), "must survive failed copy")

            # 6) Read-only file and read-only folder purge.
            ro_dir = ws / "ro_folder"
            ro_dir.mkdir()
            ro_child = ro_dir / "pack.idx"
            ro_child.write_text("readonly pack", encoding="utf-8")
            os.chmod(ro_child, stat.S_IREAD)
            trash.put(ws, "ro_folder")
            os.chmod(fail_file, stat.S_IREAD)
            trash.put(ws, "keep_me.txt")
            self.assertEqual(trash.purge(everything=True), 2)
            self.assertEqual(trash.list(), [])

    def test_17_unmeterable_secondary_output_does_not_deadlock_wake(self):
        meter = audio_meter.PlaybackMeter.__new__(audio_meter.PlaybackMeter)
        meter.refresh_at = float("inf")
        meter.meters = ["m1"]
        meter.default_meter = "m_default"
        meter.complete = False  # An unrelated secondary output was unmeterable.

        with mock.patch.object(audio_meter, "method") as method_mock:
            def fake_zero(_m, out_ptr):
                ctypes.cast(out_ptr, ctypes.POINTER(ctypes.c_float))[0] = 0.0
                return 0

            method_mock.return_value = fake_zero
            # Conservative peak() still reports None for unknown secondary device...
            self.assertIsNone(meter.peak())
            # ...while wake_playback_peak() checks the default speaker and sees silence (0.0).
            self.assertEqual(meter.wake_playback_peak(), 0.0)

            def fake_playing(_m, out_ptr):
                ctypes.cast(out_ptr, ctypes.POINTER(ctypes.c_float))[0] = 0.25
                return 0

            method_mock.return_value = fake_playing
            self.assertAlmostEqual(meter.wake_playback_peak(), 0.25, places=5)

            meter.default_meter = None
            method_mock.return_value = fake_zero
            self.assertIsNone(meter.wake_playback_peak())

    def test_18_nonblocking_clock_chat_context_and_smart_task_escalation(self):
        # Sri Lanka clock computes in pure Python without spawning PowerShell.
        with mock.patch.object(JEAN_DESKTOP.subprocess, "run") as run_mock:
            answer = JEAN_DESKTOP.clock_answer("sri-lanka")
            self.assertIn("in Sri Lanka.", answer)
            run_mock.assert_not_called()

        overlay = JEAN_DESKTOP.Overlay()
        try:
            # Rolling chat context passed on second turn and cleared by "new conversation".
            calls = []
            done = threading.Event()

            def fake_chat_jean(argv, timeout, cancel=None, env=None, use_saved_models=True):
                calls.append(list(argv))
                done.set()
                return "Hello Rashen, standing by.", ""

            with (
                mock.patch.object(overlay, "_jean", side_effect=fake_chat_jean),
                mock.patch.object(overlay, "_speak_async"),
            ):
                overlay.mode = "chat"
                overlay.busy = True
                overlay._run_chat("Hi JEAN")
                self.assertTrue(done.wait(timeout=3))
                self.app.processEvents()
                self.assertEqual(len(overlay.chat_history), 2)

                done.clear()
                overlay.busy = True
                overlay._run_chat("What did I just say?")
                self.assertTrue(done.wait(timeout=3))
                self.app.processEvents()
                self.assertIn("--context", calls[1])
                ctx_val = calls[1][calls[1].index("--context") + 1]
                self.assertIn("User: Hi JEAN", ctx_val)

                overlay.busy = True
                overlay._submit_request("chat", "new conversation")
                self.assertEqual(overlay.chat_history, [])

            # Smart-mode auto-escalation when chat reply indicates "run as a task".
            escalated = []
            esc_done = threading.Event()

            def fake_escalate_jean(argv, timeout, cancel=None, env=None, use_saved_models=True):
                return "I can help with that — please run as a task.", ""

            def on_escalate(task_text):
                escalated.append(task_text)
                esc_done.set()

            with (
                mock.patch.object(overlay, "_jean", side_effect=fake_escalate_jean),
                mock.patch.object(overlay, "_run_task", side_effect=on_escalate),
                mock.patch.object(overlay, "_speak_async"),
            ):
                overlay.mode = "smart"
                overlay.busy = True
                overlay._run_chat("check my memory pressure")
                deadline = JEAN_DESKTOP.time.monotonic() + 3.0
                while not esc_done.is_set() and JEAN_DESKTOP.time.monotonic() < deadline:
                    self.app.processEvents()
                    JEAN_DESKTOP.time.sleep(0.01)
                self.assertEqual(escalated, ["check my memory pressure"])

            # Wake and polite prefixes are stripped for task delegation commands.
            with mock.patch.object(overlay, "_run_task") as run_task_mock:
                overlay._submit_request("task", "Jean, please start with Codex. Fix the test")
                run_task_mock.assert_called_once_with("start with Codex. Fix the test")
        finally:
            overlay.close()

    def test_19_jarvis_voice_flow_spoken_memory_dreaming_and_playbooks(self):
        overlay = JEAN_DESKTOP.Overlay()
        try:
            # 1) Spoken replies read real memory content cleanly without raw IDs.
            overlay.voice_reply = ["fallback"]
            overlay.show_result("Local memory", "f-abc123\tscope=global\tcreated=1 updated=1\tOperator prefers Rust")
            self.assertEqual(overlay.voice_reply[0], "Operator prefers Rust")

            overlay.voice_reply = ["fallback"]
            overlay.show_result("Dream cycle complete", "Dream cycle (deterministic): scanned 4 episodes; created 2 notes.")
            self.assertIn("scanned 4 episodes", overlay.voice_reply[0])
            overlay.voice_reply = None

            # 2) Memory recall, self-update, offline dream, deep dream, and playbooks routing.
            calls = []
            done = threading.Event()

            def fake_jean(argv, timeout, cancel=None, env=None, use_saved_models=True):
                calls.append((list(argv), use_saved_models))
                done.set()
                return "Dream cycle complete.", ""

            with (
                mock.patch.object(overlay, "_jean", side_effect=fake_jean),
                mock.patch.object(overlay, "_speak_async"),
            ):
                for phrase, expected_sub, expected_models in (
                    ("Jean, what do you remember about Rust?", "recall", False),
                    ("Update memory: Actually, my editor is Cursor", "observe", False),
                    ("Jean, dream now", "dream", False),
                    ("deep dream", "dream", True),
                ):
                    calls.clear()
                    done.clear()
                    self.assertTrue(overlay._handle_memory(phrase), f"unhandled: {phrase}")
                    self.assertTrue(done.wait(timeout=3))
                    self.app.processEvents()
                    self.assertIn(expected_sub, calls[0][0])
                    self.assertEqual(calls[0][1], expected_models)

                # Wait for background work_finished signal to settle before idle dream check.
                self.app.processEvents()
                # Idle dreaming triggers off-thread after 3+ turns when idle.
                calls.clear()
                done.clear()
                overlay.busy = False
                overlay.dream_running = False
                overlay.pending_dream_turns = 3
                overlay._maybe_idle_dream()
                self.assertTrue(done.wait(timeout=3))
                self.app.processEvents()
                self.assertIn("dream", calls[0][0])
                self.assertEqual(overlay.pending_dream_turns, 0)

            with mock.patch.object(overlay, "_run_task") as run_task_mock:
                self.assertTrue(overlay._handle_workflow("Jean, list playbooks"))
                run_task_mock.assert_called_once_with("/playbook list")
                run_task_mock.reset_mock()
                self.assertTrue(overlay._handle_workflow("run playbook morning-check"))
                run_task_mock.assert_called_once_with("/playbook run morning-check")

            # 3) JARVIS conversational voice turn: empty miss retries once; standby phrase ends cleanly.
            with (
                mock.patch.object(overlay, "_speak_async") as speak_mock,
            ):
                t1 = JEAN_DESKTOP.VoiceTurn("smart", "project9-whisper", None, tempfile.TemporaryDirectory(), conversational=True)
                overlay.voice_turn = t1
                overlay.voice_session = True
                overlay._on_decoded(t1, "", "", 100)
                self.assertTrue(overlay.voice_session, "first empty miss should keep conversation open for one retry")
                self.assertEqual(overlay.voice_misses, 1)

                t2 = JEAN_DESKTOP.VoiceTurn("smart", "project9-whisper", None, tempfile.TemporaryDirectory(), conversational=True)
                overlay.voice_turn = t2
                overlay._on_decoded(t2, "Thank you Jean", "", 100)
                self.assertFalse(overlay.voice_session, "thank you should gracefully enter standby")
                self.assertIn("Standing by", speak_mock.call_args[0][0])
        finally:
            overlay.close()

    def test_20_overlay_repaint_safety_and_non_clipping_resizes(self):
        from PyQt6.QtCore import Qt

        overlay = JEAN_DESKTOP.Overlay()
        try:
            # No QGraphicsDropShadowEffect offscreen proxy on the translucent window card.
            self.assertIsNone(overlay.card.graphicsEffect())
            self.assertTrue(bool(overlay.windowFlags() & Qt.WindowType.NoDropShadowWindowHint))
            self.assertTrue(bool(overlay.detail.alignment() & Qt.AlignmentFlag.AlignTop))

            overlay.toggle_message()
            overlay.show()
            self.app.processEvents()
            initial_card_w = overlay.card.width()

            # Simulate listening -> speaking with multi-line replies that previously clipped text
            # and pushed the card upward into the Reactor animation area.
            overlay.talk.setText("■ End recording")
            overlay.show_result(
                "Microphone is listening",
                "Speak now or say “Stop JEAN” to end the conversation.",
            )
            self.app.processEvents()
            self.assertEqual(overlay.card.width(), initial_card_w)

            overlay.talk.setText("● Talk")
            overlay.status.setText("●  SPEAKING")
            for text in (
                "JEAN. Glad to hear you're happy, sir. I'll keep the systems running smoothly — just give me the word when you need something done.",
                "Line one verifies a longer five-line spoken reply while JEAN speaks aloud on Windows. "
                "Line two checks that the card expands smoothly downward without shifting the ring or overlapping the reactor. "
                "Line three ensures crisp text.",
            ):
                overlay.show_result("JEAN", text)
                self.app.processEvents()
                reactor_bottom = overlay.reactor.y() + overlay.reactor.height()
                self.assertGreaterEqual(
                    overlay.card.y(),
                    reactor_bottom + 4,
                    "card must stay cleanly separated below the animating Reactor",
                )
                needed_detail_h = min(
                    overlay.detail.heightForWidth(overlay.detail.width()),
                    overlay.detail.maximumHeight(),
                )
                self.assertGreaterEqual(
                    overlay.detail.height(),
                    needed_detail_h,
                    "multi-line detail text must not be vertically clipped",
                )
                self.assertGreaterEqual(
                    overlay.entry.height(),
                    38,
                    "input QLineEdit must not be vertically squished",
                )
                self.assertEqual(
                    overlay.card.width(),
                    initial_card_w,
                    "long wrapped replies must not jitter the card width",
                )

            # Opening and closing a section preserves reactor gap and clean sizing.
            overlay._toggle_section("activity", True)
            self.app.processEvents()
            self.assertGreaterEqual(
                overlay.card.y(),
                overlay.reactor.y() + overlay.reactor.height() + 4,
            )
            overlay._toggle_section("activity", False)
            self.app.processEvents()
            self.assertGreaterEqual(
                overlay.detail.height(),
                min(overlay.detail.heightForWidth(overlay.detail.width()), overlay.detail.maximumHeight()),
            )
        finally:
            overlay.close()

    def test_21_browser_origin_approval_and_youtube_bing_routing(self):
        overlay = JEAN_DESKTOP.Overlay()
        try:
            # 1) Unlisted browser origin approval is NEVER auto-approved by YOLO mode.
            sent_inputs = []
            overlay.task_proc = object()
            overlay.cancel_sent = False
            overlay.yolo.setChecked(True)
            with mock.patch.object(overlay, "_send_task_input", side_effect=sent_inputs.append):
                overlay._ask_approval({
                    "id": 21,
                    "summary": "allow unlisted browser origin https://www.youtube.com (outside [browser] allowed_origins; redirects to other origins remain blocked)",
                    "operation": "browser_origin_access",
                    "tool": "browser.local",
                    "tier": "normal",
                    "capabilities": ["network_egress_new_domain", "browser.drive"],
                    "timeout_ms": 30000,
                })
                self.assertEqual(sent_inputs, [], "YOLO must not auto-approve browser_origin_access")
                self.assertIsNotNone(overlay.approval)
                self.assertIn("https://www.youtube.com", overlay.approval.text())
                self.assertIn("AWAITING APPROVAL", overlay.status.text())
                overlay._answer_approval("task approved", overlay.approval)
                self.app.processEvents()
                self.assertEqual(sent_inputs, [{"kind": "approve", "id": 21, "allow": True}])
            overlay.task_proc = None

            # 2) YouTube playback and Bing search route through _run_task (controlled browser + approval + verification).
            with (
                mock.patch.object(overlay, "_run_task") as run_task_mock,
                mock.patch.object(JEAN_DESKTOP.QDesktopServices, "openUrl") as open_url_mock,
            ):
                for phrase in (
                    "Play Me at the zoo on YouTube",
                    "Open YouTube and play lofi beats",
                    "Play https://www.youtube.com/watch?v=jNQXAC9IVRw",
                    "Search rust programming on Bing",
                    "Open Bing",
                ):
                    run_task_mock.reset_mock()
                    open_url_mock.reset_mock()
                    overlay._submit_request("task", phrase)
                    run_task_mock.assert_called_once_with(phrase)
                    open_url_mock.assert_not_called()

                run_task_mock.reset_mock()
                overlay._browse("--verify-video https://www.youtube.com/watch?v=jNQXAC9IVRw")
                run_task_mock.assert_called_once_with(
                    "/browse --verify-video https://www.youtube.com/watch?v=jNQXAC9IVRw"
                )
        finally:
            overlay.close()

    def test_22_gui_efficiency_settings_persistence_and_yolo_guard(self):
        from PyQt6.QtCore import QSettings

        with tempfile.TemporaryDirectory(prefix="jean-gui-settings-") as tmp:
            ini_path = Path(tmp) / "desktop.ini"
            def make_settings(*_args, **_kwargs):
                return QSettings(str(ini_path), QSettings.Format.IniFormat)

            with mock.patch.object(JEAN_DESKTOP, "QSettings", side_effect=make_settings):
                overlay = JEAN_DESKTOP.Overlay()
                try:
                    # 1) Efficiency: when OS RegisterHotKey succeeds, the 50 Hz fallback polling timer stays stopped.
                    if overlay.hotkey_registered:
                        self.assertFalse(
                            overlay.hotkey_timer.isActive(),
                            "hotkey polling timer should stay off when RegisterHotKey succeeded",
                        )

                    # 2) Efficiency: when Wake is Off and not speaking, tick() does not poll COM playback meter.
                    fake_meter = mock.MagicMock()
                    overlay.playback_meter = fake_meter
                    overlay.speaking_proc = None
                    overlay.wake_button.setChecked(False)
                    overlay.tick()
                    fake_meter.wake_playback_peak.assert_not_called()
                    fake_meter.peak.assert_not_called()

                    # 3) Safety: toggling YOLO on while a browser_origin_access dialog is open must NOT auto-approve it.
                    sent_inputs = []
                    overlay.task_proc = object()
                    overlay.cancel_sent = False
                    overlay.yolo.setChecked(False)
                    with mock.patch.object(overlay, "_send_task_input", side_effect=sent_inputs.append):
                        overlay._ask_approval({
                            "id": 99,
                            "summary": "allow unlisted browser origin https://www.bing.com",
                            "operation": "browser_origin_access",
                            "tool": "browser.local",
                            "tier": "normal",
                            "capabilities": ["network_egress_new_domain", "browser.drive"],
                            "timeout_ms": 30000,
                        })
                        overlay.yolo.setChecked(True)
                        self.app.processEvents()
                        self.assertEqual(
                            sent_inputs,
                            [],
                            "_set_yolo must never auto-approve an open browser_origin_access dialog",
                        )
                        if overlay.approval is not None:
                            overlay.approval.reject()
                            self.app.processEvents()
                    overlay.task_proc = None

                    # 4) Unified SettingsDialog: clean 4 tabs, persists preferences, respects quiet mode & idle dream toggle.
                    overlay.show_settings()
                    self.app.processEvents()
                    dlg = overlay.settings_dialog
                    self.assertIsNotNone(dlg)
                    self.assertEqual(dlg.tabs.count(), 4)
                    self.assertEqual(
                        [dlg.tabs.tabText(i).replace("&&", "&") for i in range(4)],
                        ["General", "Voice & Audio", "Safety & Tools", "Memory & Playbooks"],
                    )
                    overlay.set_mode("task")
                    overlay.set_speak_replies(False)
                    overlay.set_brief_speech(False)
                    overlay.set_idle_dream_enabled(False)
                    overlay.set_workspace_path(tmp)

                    # Quiet mode suppresses spoken output unless force=True (speaker test).
                    with mock.patch.object(JEAN_DESKTOP.subprocess, "Popen") as popen_mock:
                        overlay._speak_async("Hello silent mode", False)
                        popen_mock.assert_not_called()

                    # Idle dream disabled suppresses idle dreaming even with 3+ pending turns.
                    with mock.patch.object(overlay, "_jean") as jean_mock:
                        overlay.pending_dream_turns = 5
                        overlay._maybe_idle_dream()
                        jean_mock.assert_not_called()

                    dlg.close()
                finally:
                    overlay.close()

                # 5) Restarting Overlay loads all persisted settings cleanly.
                overlay2 = JEAN_DESKTOP.Overlay()
                try:
                    self.assertEqual(overlay2.mode, "task")
                    self.assertFalse(overlay2.speak_replies)
                    self.assertFalse(overlay2.brief_speech)
                    self.assertFalse(overlay2.idle_dream_enabled)
                    self.assertFalse(overlay2.wake_button.isChecked())
                    self.assertEqual(overlay2.workspace.text(), tmp)
                finally:
                    overlay2.close()


if __name__ == "__main__":
    unittest.main()
